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

use merge::{ConflictMarkers, MergeOutcome, text_hunk_merge_with_markers};

use super::items::{FileSegments, Item, ItemKey, ItemKind, inter_ranges};

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

#[derive(Default)]
struct MovedOutMethods<'a> {
    ours: BTreeMap<ItemKey, MovedOutMethod<'a>>,
    theirs: BTreeMap<ItemKey, MovedOutMethod<'a>>,
}

#[derive(Clone, Copy)]
struct MovedOutMethod<'a> {
    base_inline: &'a Item,
    opposite_inline: Option<&'a Item>,
}

impl<'a> MovedOutMethods<'a> {
    fn detect(
        base_segments: &'a FileSegments,
        ours_segments: &'a FileSegments,
        theirs_segments: &'a FileSegments,
    ) -> Self {
        Self {
            ours: detect_moved_out_methods(base_segments, ours_segments, theirs_segments),
            theirs: detect_moved_out_methods(base_segments, theirs_segments, ours_segments),
        }
    }

    fn virtualize_top_level(
        &self,
        depth: usize,
        key: &ItemKey,
        mut base_item: Option<&'a Item>,
        mut ours_item: Option<&'a Item>,
        mut theirs_item: Option<&'a Item>,
    ) -> (Option<&'a Item>, Option<&'a Item>, Option<&'a Item>) {
        if depth != 0 || base_item.is_some() {
            return (base_item, ours_item, theirs_item);
        }
        if let Some(moved) = self.ours.get(key)
            && ours_item.is_some()
        {
            base_item = Some(moved.base_inline);
            if theirs_item.is_none() {
                theirs_item = moved.opposite_inline;
            }
        }
        if let Some(moved) = self.theirs.get(key)
            && theirs_item.is_some()
        {
            base_item = Some(moved.base_inline);
            if ours_item.is_none() {
                ours_item = moved.opposite_inline;
            }
        }
        (base_item, ours_item, theirs_item)
    }

    fn consumes_nested_inline(
        &self,
        key: &ItemKey,
        base_item: Option<&Item>,
        ours_item: Option<&Item>,
        theirs_item: Option<&Item>,
    ) -> bool {
        base_item.is_some()
            && ((self.ours.contains_key(key) && ours_item.is_none())
                || (self.theirs.contains_key(key) && theirs_item.is_none()))
    }
}

fn detect_moved_out_methods<'a>(
    base_segments: &'a FileSegments,
    moving_segments: &'a FileSegments,
    opposite_segments: &'a FileSegments,
) -> BTreeMap<ItemKey, MovedOutMethod<'a>> {
    let mut moved = BTreeMap::new();
    for item in moving_segments.items.iter().filter(|item| {
        item.body.is_none() && item.key.kind == ItemKind::Function && !item.key.scope.is_empty()
    }) {
        if find_nested_item(&moving_segments.items, &item.key).is_some() {
            continue;
        }
        let Some(base_inline) = find_nested_item(&base_segments.items, &item.key) else {
            continue;
        };
        moved.insert(
            item.key.clone(),
            MovedOutMethod {
                base_inline,
                opposite_inline: find_nested_item(&opposite_segments.items, &item.key),
            },
        );
    }
    moved
}

fn find_nested_item<'a>(items: &'a [Item], key: &ItemKey) -> Option<&'a Item> {
    for item in items {
        if let Some(body) = &item.body
            && let Some(found) = find_item(&body.items, key)
        {
            return Some(found);
        }
    }
    None
}

fn find_item<'a>(items: &'a [Item], key: &ItemKey) -> Option<&'a Item> {
    for item in items {
        if item.key == *key {
            return Some(item);
        }
        if let Some(body) = &item.body
            && let Some(found) = find_item(&body.items, key)
        {
            return Some(found);
        }
    }
    None
}

/// Stitch three sides together via recursive per-region tree merge.
///
/// The whole file is the outermost region; each matched container body is a
/// nested region merged the same way ([`merge_region`]). The trailing-newline
/// reconcile + outcome wrapping happen once, here, around the top-level merge.
pub(crate) fn reconstruct_merged_file(
    base: &str,
    ours: &str,
    theirs: &str,
    base_segments: &FileSegments,
    ours_segments: &FileSegments,
    theirs_segments: &FileSegments,
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    // Whole-file source bundle: lets `resolve_item` slice per-item bytes AND
    // carries a whole-file `EolPolicy` used by the trailing newline path
    // (`reconcile_trailing_newline`) and as a fallback by the marker path
    // (`emit_addadd_conflict`) when the conflicting item bodies carry zero
    // EOL observations (Codex r8, cid 3256283857).
    let sides = SideSources::new(base, ours, theirs);
    let moved_out = MovedOutMethods::detect(base_segments, ours_segments, theirs_segments);

    let (mut output, total_conflicts) = merge_region(
        sides,
        &moved_out,
        0,
        &base_segments.items,
        &ours_segments.items,
        &theirs_segments.items,
        (0, base_segments.source_len),
        (0, ours_segments.source_len),
        (0, theirs_segments.source_len),
        markers,
    );

    reconcile_trailing_newline(&mut output, sides);

    if total_conflicts == 0 {
        MergeOutcome::Clean(output)
    } else {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers: output,
            conflict_count: total_conflicts,
        }
    }
}

/// Merge one *region* — a list of sibling items occupying `[start, end)` on
/// each side — into a byte string + conflict count. Called on the whole file
/// at top level and, recursively (via [`resolve_container`]), on each matched
/// container body. Recursion depth is bounded by the container-nesting cap in
/// [`super::items`], so it cannot overflow the stack.
///
/// The algorithm is the heddle#68/#468 weave, generalized to a region:
/// resolve each item by `MatchKey`, compute an emit order, and weave the
/// per-side inter-item segments (including a container body's own braces,
/// which live in its region's preamble/postamble) back between the items.
#[allow(clippy::too_many_arguments)]
fn merge_region<'items>(
    sides: SideSources<'_>,
    moved_out: &MovedOutMethods<'items>,
    depth: usize,
    base_items: &'items [Item],
    ours_items: &'items [Item],
    theirs_items: &'items [Item],
    base_bounds: (usize, usize),
    ours_bounds: (usize, usize),
    theirs_bounds: (usize, usize),
    markers: ConflictMarkers<'_>,
) -> (Vec<u8>, usize) {
    // Per-side match keys in source order — (ItemKey, discriminator) tuples.
    // Leaves use a positional occurrence index; container instances are
    // aligned to base by child-key overlap so a prepended/appended/reordered
    // same-name container keeps an identity distinct from the matched base
    // block (the heddle#484 r3 cross-side class). See [`build_aligned_match_keys`].
    let (base_mks, ours_mks, theirs_mks) =
        build_aligned_match_keys(base_items, ours_items, theirs_items, sides);

    let base_map: BTreeMap<MatchKey, &Item> = base_mks
        .iter()
        .zip(base_items.iter())
        .map(|(mk, i)| (mk.clone(), i))
        .collect();
    let ours_map: BTreeMap<MatchKey, &Item> = ours_mks
        .iter()
        .zip(ours_items.iter())
        .map(|(mk, i)| (mk.clone(), i))
        .collect();
    let theirs_map: BTreeMap<MatchKey, &Item> = theirs_mks
        .iter()
        .zip(theirs_items.iter())
        .map(|(mk, i)| (mk.clone(), i))
        .collect();

    let all_keys: BTreeSet<&MatchKey> = base_map
        .keys()
        .chain(ours_map.keys())
        .chain(theirs_map.keys())
        .collect();

    let mut resolved: BTreeMap<MatchKey, (Option<Vec<u8>>, usize)> = BTreeMap::new();
    let mut total_conflicts = 0usize;

    // Non-`use` items: per-item positional resolution, matched by
    // (key, occurrence). A matched *container* recurses into its body via
    // `resolve_node` → `resolve_container`; a leaf is a byte 3-way merge.
    // `use` items are skipped here — their content is NEVER decided by
    // positional occurrence index (the heddle#468 r5 bug class). They are
    // resolved below as whole leaf-components.
    for key in &all_keys {
        if key.0.kind == ItemKind::Use {
            continue;
        }
        let base_item = base_map.get(*key).copied();
        let ours_item = ours_map.get(*key).copied();
        let theirs_item = theirs_map.get(*key).copied();
        let resolution = if depth > 0
            && moved_out.consumes_nested_inline(&key.0, base_item, ours_item, theirs_item)
        {
            (None, 0)
        } else {
            let (base_item, ours_item, theirs_item) =
                moved_out.virtualize_top_level(depth, &key.0, base_item, ours_item, theirs_item);
            resolve_node(
                sides,
                moved_out,
                depth,
                base_item,
                ours_item,
                theirs_item,
                markers,
            )
        };
        total_conflicts += resolution.1;
        resolved.insert((*key).clone(), resolution);
    }

    // `use` items at THIS level: resolve each canonical leaf-component as ONE
    // set-valued unit (the heddle#468 r5 fix). After `canonicalize_use_keys`,
    // every declaration in a component shares one `ItemKey`; comparing full
    // component leaf-SETS rather than occurrence positions makes the
    // duplicate-import class impossible. Components are scoped to the region
    // because the `ItemKey` carries the enclosing scope, so uses in different
    // containers never group together.
    let mut use_components: BTreeMap<ItemKey, [Vec<&Item>; 3]> = BTreeMap::new();
    for (side, items) in [base_items, ours_items, theirs_items].iter().enumerate() {
        for item in *items {
            if item.key.kind == ItemKind::Use {
                use_components
                    .entry(item.key.clone())
                    .or_insert_with(|| [Vec::new(), Vec::new(), Vec::new()])[side]
                    .push(item);
            }
        }
    }
    for (key, [base_uses, ours_uses, theirs_uses]) in &use_components {
        let (bytes, conflicts) =
            resolve_use_component(sides, base_uses, ours_uses, theirs_uses, markers);
        total_conflicts += conflicts;
        resolved.insert((key.clone(), 0), (bytes, conflicts));
        // Higher-occurrence slots of this component exist only so the
        // inter-item segment weaver can place the surrounding whitespace;
        // they carry no item bytes (the verdict above is the whole unit).
        let slots = base_uses.len().max(ours_uses.len()).max(theirs_uses.len());
        for occ in 1..slots {
            resolved.insert((key.clone(), occ), (None, 0));
        }
    }

    let item_emit_order = compute_item_emit_order(&base_mks, &ours_mks, &theirs_mks, &all_keys);

    let side_idx_maps = [
        match_key_index(&base_mks),
        match_key_index(&ours_mks),
        match_key_index(&theirs_mks),
    ];
    let side_ranges = [
        inter_ranges(base_items, base_bounds.0, base_bounds.1),
        inter_ranges(ours_items, ours_bounds.0, ours_bounds.1),
        inter_ranges(theirs_items, theirs_bounds.0, theirs_bounds.1),
    ];
    let side_sources = [sides.base, sides.ours, sides.theirs];

    // Per-side set of inter-item range indices already emitted. A side's
    // range is contributed to at most one slot (see the Codex r2 P2 #2 /
    // P1 #2 duplication shapes the tracking prevents).
    let mut emitted: [BTreeSet<usize>; N_SIDES] =
        [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];

    let mut output: Vec<u8> = Vec::new();

    for key in &item_emit_order {
        let Some((item_resolution, _)) = resolved.get(key) else {
            continue;
        };
        if item_resolution.is_none() && key.0.kind != ItemKind::Use {
            continue;
        }

        let mut segs: [Option<&str>; N_SIDES] = [None, None, None];
        for s in 0..N_SIDES {
            // A side contributes the gap PRECEDING `key` only if it actually
            // has `key`. A side that lacks `key` (an item added on another
            // side, or one this side deleted) contributes nothing for this
            // slot — its surrounding content flows with its own items and its
            // trailing content stays in the postamble. Bridging a lacking
            // side to "the gap after its nearest prior item" used to pull the
            // postamble (or a mid-sequence gap) in early, duplicating it —
            // the heddle#484 Bug 1 (`// MARK` woven twice) / Bug 2 (module
            // duplicated) class.
            if let Some(&r) = side_idx_maps[s].get(key)
                && emitted[s].insert(r)
            {
                segs[s] = Some(inter_slice(side_sources[s], &side_ranges[s], r));
            }
        }
        let (seg_bytes, seg_conflicts) = merge_segment(segs[0], segs[1], segs[2], markers);
        output.extend_from_slice(&seg_bytes);
        total_conflicts += seg_conflicts;

        if let Some(item_bytes) = item_resolution {
            output.extend_from_slice(item_bytes);
        }
    }

    // Postamble: each side's last range (a container body's closing brace +
    // indentation lives here), but only if it wasn't already pulled in as a
    // bridge above and adds bytes (avoids duplicating a trailing newline
    // already in the last item's bytes — the top-level P1 #2 shape).
    let mut post: [Option<&str>; N_SIDES] = [None, None, None];
    for s in 0..N_SIDES {
        let last = side_ranges[s].len() - 1;
        if emitted[s].insert(last) {
            post[s] = Some(inter_slice(side_sources[s], &side_ranges[s], last));
        }
    }
    let (post_bytes, post_conflicts) = merge_segment(post[0], post[1], post[2], markers);
    if !post_bytes.is_empty() {
        output.extend_from_slice(&post_bytes);
    }
    total_conflicts += post_conflicts;

    (output, total_conflicts)
}

/// Walk a side's items in source order and tag each with its per-key
/// occurrence index — 0 for the first item with that key, 1 for the second,
/// and so on. Length matches `items.len()`.
fn build_match_keys(items: &[Item]) -> Vec<MatchKey> {
    let mut counters: BTreeMap<ItemKey, usize> = BTreeMap::new();
    items
        .iter()
        .map(|item| {
            let n = counters.entry(item.key.clone()).or_insert(0);
            let occurrence = *n;
            *n += 1;
            (item.key.clone(), occurrence)
        })
        .collect()
}

/// Immediate child-key set of a container item (empty for leaves). Used to
/// align container instances across sides by content overlap.
fn child_key_set(item: &Item) -> BTreeSet<&ItemKey> {
    match &item.body {
        Some(body) => body.items.iter().map(|c| &c.key).collect(),
        None => BTreeSet::new(),
    }
}

/// Build the three sides' `MatchKey` lists with cross-side-consistent
/// discriminators.
///
/// Leaves get a per-side positional occurrence index — base's first `foo`
/// pairs with ours's first `foo`, the existing heddle#68 scheme. **Container
/// instances** (same `(kind, name, scope)`, multiple blocks) anchor to base in
/// two passes:
///
/// 1. **Content-overlap (primary).** Each ours/theirs block is paired with the
///    unused base block whose immediate child-keys it most overlaps (overlap
///    `> 0`), inheriting that base block's discriminator. Positional occurrence
///    alone mis-pairs a prepended `impl Foo` (occurrence 0) with base's
///    `impl Foo` (occurrence 0) — the r3 collapse; overlap alignment pairs by
///    what the block actually contains, so identity survives reordering.
/// 2. **Header-anchored positional fallback (no content signal).** A block that
///    found no overlap — an EMPTY container, or one whose children were fully
///    replaced — aligns to the UNUSED base block of its key whose *header*
///    bytes (`[start_byte, body.inner_start)`, which absorb leading metadata:
///    attributes, decorators, and separator comments — see
///    [`super::items::leading_metadata_start`]) byte-match it; failing a header
///    match, the next unused base block in source order. Only when no unused
///    base block of that key remains is it a genuinely-new container → fresh
///    discriminator above the base range. The header anchor is what pins a
///    *surviving* zero-overlap block to its TRUE base occurrence even when an
///    earlier same-key block was deleted on that side: "next unused" alone
///    grabs the earliest free slot (0), so deleting the first block and editing
///    the second mis-mapped the survivor to slot 0, treated slot 1 as deleted,
///    and wove its separator/trivia onto the wrong block (heddle#490 r2 /
///    Codex P1). Without *any* fallback an empty same-key container minted
///    fresh, its base slot resolved as deleted, and a clean one-sided edit
///    corrupted (heddle#490 r1).
///
/// For leaves (no children) every overlap is 0 and the leaf path applies a
/// plain positional occurrence index directly.
fn build_aligned_match_keys(
    base: &[Item],
    ours: &[Item],
    theirs: &[Item],
    sides: SideSources<'_>,
) -> (Vec<MatchKey>, Vec<MatchKey>, Vec<MatchKey>) {
    // A key is a "container key" if any instance on any side carries a body.
    let mut container_keys: BTreeSet<ItemKey> = BTreeSet::new();
    for items in [base, ours, theirs] {
        for it in items {
            if it.body.is_some() {
                container_keys.insert(it.key.clone());
            }
        }
    }

    // Base is the anchor: plain positional occurrence (its discriminator for a
    // container is its source-order index within the key group).
    let base_mks = build_match_keys(base);

    let align = |side: &[Item], side_src: &str| -> Vec<MatchKey> {
        // base container instances per key: (base discriminator, &base item),
        // in base source order (so the positional fallback below scans the
        // *earliest* unused base slot first).
        let mut base_by_key: BTreeMap<&ItemKey, Vec<(usize, &Item)>> = BTreeMap::new();
        for (i, it) in base.iter().enumerate() {
            if container_keys.contains(&it.key) {
                base_by_key
                    .entry(&it.key)
                    .or_default()
                    .push((base_mks[i].1, it));
            }
        }
        let mut used: BTreeMap<&ItemKey, BTreeSet<usize>> = BTreeMap::new();
        let mut leaf_occ: BTreeMap<&ItemKey, usize> = BTreeMap::new();
        let mut fresh: BTreeMap<&ItemKey, usize> = BTreeMap::new();

        // Container discriminators are decided in TWO passes so that
        // content-overlap stays the PRIMARY signal: a zero-overlap (e.g. empty)
        // block must never greedily claim a base slot that a later
        // content-overlap block would match. `disc_of[pos]` holds the
        // pass-1 verdict per side position; `None` = deferred to pass 2.
        let mut disc_of: Vec<Option<usize>> = vec![None; side.len()];

        // Pass 1 — content-overlap alignment (PRIMARY). Each container claims
        // the unused base candidate of its key with the greatest immediate
        // child-key overlap, when that overlap is > 0. This is the heddle#484
        // r3 mechanism: a reordered/edited non-empty block matches by what it
        // actually contains.
        for (pos, it) in side.iter().enumerate() {
            if !container_keys.contains(&it.key) {
                continue;
            }
            let childset = child_key_set(it);
            let used_set = used.entry(&it.key).or_default();
            let mut best: Option<(usize, usize)> = None; // (overlap, base disc)
            if let Some(cands) = base_by_key.get(&it.key) {
                for (disc, bitem) in cands {
                    if used_set.contains(disc) {
                        continue;
                    }
                    let overlap = childset.intersection(&child_key_set(bitem)).count();
                    if overlap > 0 && best.is_none_or(|(o, _)| overlap > o) {
                        best = Some((overlap, *disc));
                    }
                }
            }
            if let Some((_, d)) = best {
                used_set.insert(d);
                disc_of[pos] = Some(d);
            }
        }

        // Pass 2a — header-anchored alignment for the no-content-signal case.
        // A container left unresolved by pass 1 (an EMPTY container, or one
        // whose children were fully replaced so it overlaps no base block)
        // claims the earliest UNUSED base candidate of its key whose HEADER
        // bytes byte-match it. The header is `[start_byte, body.inner_start)`,
        // which absorbs the block's leading metadata — attributes, decorators,
        // and separator comments (see [`super::items::leading_metadata_start`])
        // — so a SURVIVING block that kept its preceding comment re-anchors to
        // the exact base occurrence that comment belonged to. This pins the
        // survivor to its TRUE base slot even when an earlier same-key block was
        // deleted on this side, and even under a reorder: "next unused" alone
        // grabbed slot 0 and wove the deleted slot's separator onto the survivor
        // (heddle#490 r2). Header matching runs as a PRIORITY pass — before the
        // source-order scan below — so a newly-prepended block (no header match)
        // can't greedily steal the slot a surviving block needs.
        for (pos, it) in side.iter().enumerate() {
            if !container_keys.contains(&it.key) || disc_of[pos].is_some() {
                continue;
            }
            let it_header = align_header_bytes(it, side_src);
            let used_set = used.entry(&it.key).or_default();
            if let Some(cands) = base_by_key.get(&it.key)
                && let Some((d, _)) = cands.iter().find(|(d, b)| {
                    !used_set.contains(d) && align_header_bytes(b, sides.base) == it_header
                })
            {
                used_set.insert(*d);
                disc_of[pos] = Some(*d);
            }
        }

        // Pass 2b — source-order fallback + leaves. Any container still
        // unresolved (indistinguishable headers, or no header match) claims the
        // next UNUSED base candidate of its key in base source order; only when
        // none remains is it a genuinely-new container beyond base's count →
        // fresh discriminator above the base range. When headers are
        // indistinguishable, either slot choice is byte-equivalent (identical
        // separators), so no corruption can result. This stays a narrow fallback
        // for the zero-overlap case, NOT a return to the per-everything ordinal
        // model. Leaves get a plain positional occurrence index.
        let mut out = Vec::with_capacity(side.len());
        for (pos, it) in side.iter().enumerate() {
            if container_keys.contains(&it.key) {
                let disc = if let Some(d) = disc_of[pos] {
                    d
                } else {
                    let used_set = used.entry(&it.key).or_default();
                    let next_unused = base_by_key.get(&it.key).and_then(|cands| {
                        cands
                            .iter()
                            .map(|(d, _)| *d)
                            .find(|d| !used_set.contains(d))
                    });
                    if let Some(d) = next_unused {
                        used_set.insert(d);
                        d
                    } else {
                        let base_count = base_by_key.get(&it.key).map_or(0, Vec::len);
                        let f = fresh.entry(&it.key).or_insert(0);
                        let d = base_count + *f;
                        *f += 1;
                        d
                    }
                };
                out.push((it.key.clone(), disc));
            } else {
                let occ = leaf_occ.entry(&it.key).or_insert(0);
                let d = *occ;
                *occ += 1;
                out.push((it.key.clone(), d));
            }
        }
        out
    };

    let ours_mks = align(ours, sides.ours);
    let theirs_mks = align(theirs, sides.theirs);
    (base_mks, ours_mks, theirs_mks)
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

/// Whole byte span of an item on a side.
fn whole_bytes<'a>(item: &Item, src: &'a str) -> &'a [u8] {
    &src.as_bytes()[item.start_byte..item.end_byte]
}

/// Header bytes of a container item: `[start_byte, body.inner_start)` — e.g.
/// `impl Foo ` (everything before the body delimiter).
fn header_bytes<'a>(item: &Item, src: &'a str) -> &'a [u8] {
    let inner_start = item.body.as_ref().expect("container").inner_start;
    &src.as_bytes()[item.start_byte..inner_start]
}

/// Header bytes used to anchor a zero-overlap container to its base occurrence
/// in pass 2 of [`build_aligned_match_keys`]. For a real container this is
/// [`header_bytes`] — everything before the body delimiter, which absorbs the
/// item's leading metadata (attributes / decorators / separator comments) and
/// so identifies which base block a surviving block came from. For an opaque
/// (too-deeply-nested) container carried as a `body: None` leaf, fall back to
/// the whole byte span so the comparison stays total instead of panicking on
/// the missing body.
fn align_header_bytes<'a>(item: &Item, src: &'a str) -> &'a [u8] {
    if item.body.is_some() {
        header_bytes(item, src)
    } else {
        whole_bytes(item, src)
    }
}

/// Footer bytes of a container item: `[body.inner_end, end_byte)` — usually
/// empty (the body node ends at the item end).
fn footer_bytes<'a>(item: &Item, src: &'a str) -> &'a [u8] {
    let inner_end = item.body.as_ref().expect("container").inner_end;
    &src.as_bytes()[inner_end..item.end_byte]
}

/// Dispatch a match-key resolution to the container or leaf path.
///
/// The structural container path ([`resolve_container`] → [`merge_container_3way`])
/// is entered ONLY when *every* side that carries this matched item is a real
/// container WITH a body. That is a hard precondition: `merge_container_3way`
/// (and the `header_bytes` / `footer_bytes` it calls) read `body.inner_start` /
/// `body.inner_end`, which a leaf does not have.
///
/// A key is *usually* consistently a container or a leaf across sides, but it
/// can be MIXED: the same `ItemKey` (kind, name, scope) names a container on one
/// side and a leaf on another, because two distinct syntactic forms share a key.
/// Concretely — a Python `class C` (container) wrapped in a decorator becomes a
/// `decorated_definition` whose `container_body` is forced to `None` (a leaf)
/// while keeping the inner class's key; a Rust `mod foo { … }` (container)
/// rewritten to `mod foo;` (a leaf, no body) keeps the same module key. Such a
/// kind-mismatch cannot merge on the tree, so it routes to a whole-item 3-way
/// text merge ([`resolve_item`], over each side's full `[start_byte, end_byte)`
/// span) — clean when the edits are disjoint, a normal conflict when they
/// overlap. Making body-presence a CHECKED precondition of structural entry —
/// rather than an `unwrap` deep inside `merge_container_3way` — closes the whole
/// "structural-merge precondition violated" panic class (heddle#490 r3 / Codex
/// P2).
fn resolve_node(
    sides: SideSources<'_>,
    moved_out: &MovedOutMethods<'_>,
    depth: usize,
    base_item: Option<&Item>,
    ours_item: Option<&Item>,
    theirs_item: Option<&Item>,
    markers: ConflictMarkers<'_>,
) -> (Option<Vec<u8>>, usize) {
    let mut present = [base_item, ours_item, theirs_item]
        .into_iter()
        .flatten()
        .peekable();
    let all_containers = present.peek().is_some() && present.all(|i| i.body.is_some());
    if all_containers {
        resolve_container(
            sides,
            moved_out,
            depth,
            base_item,
            ours_item,
            theirs_item,
            markers,
        )
    } else {
        resolve_item(sides, base_item, ours_item, theirs_item, markers)
    }
}

/// Resolve a matched *container* by merging on the tree: header + recursively
/// merged body + footer. The byte-identical fast paths (unchanged side defers,
/// both-sides-identical dedup, clean delete) avoid recursion and keep the
/// container's bytes verbatim; only a genuine cross-side divergence recurses
/// into the body. Because two distinct same-name containers are distinct
/// `MatchKey`s (by occurrence), an added/prepended/appended container is never
/// conflated with a matched one — the heddle#484 collapse class is impossible
/// by construction.
fn resolve_container(
    sides: SideSources<'_>,
    moved_out: &MovedOutMethods<'_>,
    depth: usize,
    base_item: Option<&Item>,
    ours_item: Option<&Item>,
    theirs_item: Option<&Item>,
    markers: ConflictMarkers<'_>,
) -> (Option<Vec<u8>>, usize) {
    match (base_item, ours_item, theirs_item) {
        (None, None, None) => (None, 0),
        // Added on one side only — take it verbatim.
        (None, Some(o), None) => (Some(whole_bytes(o, sides.ours).to_vec()), 0),
        (None, None, Some(t)) => (Some(whole_bytes(t, sides.theirs).to_vec()), 0),
        // Both sides added a same-key container with NO base to anchor against.
        // A recursive body merge is only sound when a BASE container exists to
        // diff each side against; with no anchor, recursing mis-weaves the
        // header/delimiters — it attaches each side's opening `{`/preamble to
        // that side's first added child, emitting `{` before BOTH children and
        // duplicating the delimiter, and the empty-base safety fallback can then
        // duplicate the whole module (heddle#490 r5). So compare the two added
        // containers as WHOLE units (header + body + footer): byte-identical →
        // both sides added the same thing, take one copy; any divergence → an
        // irreconcilable whole-container conflict. This single rule subsumes the
        // r4 divergent-header case (different header ⇒ different whole content ⇒
        // conflict). The recursive structural merge below stays for the
        // base-anchored case, where diffing against the base makes it sound.
        (None, Some(o), Some(t)) => {
            let ow = whole_bytes(o, sides.ours);
            let tw = whole_bytes(t, sides.theirs);
            if ow == tw {
                (Some(ow.to_vec()), 0)
            } else {
                (Some(emit_addadd_conflict(ow, tw, markers, sides)), 1)
            }
        }
        // Existed in base, removed on both sides → clean delete.
        (Some(_), None, None) => (None, 0),
        // Modify/delete: clean delete when the modifying side preserved base;
        // conflict (whole container) otherwise.
        (Some(b), Some(o), None) => {
            let bw = whole_bytes(b, sides.base);
            let ow = whole_bytes(o, sides.ours);
            if bw == ow {
                (None, 0)
            } else {
                materialize_outcome(text_hunk_merge_with_markers(bw, ow, &[], markers))
            }
        }
        (Some(b), None, Some(t)) => {
            let bw = whole_bytes(b, sides.base);
            let tw = whole_bytes(t, sides.theirs);
            if bw == tw {
                (None, 0)
            } else {
                materialize_outcome(text_hunk_merge_with_markers(bw, &[], tw, markers))
            }
        }
        // 3-way modify. Unchanged side defers; both-identical dedups;
        // otherwise merge header + body + footer structurally.
        (Some(b), Some(o), Some(t)) => {
            let bw = whole_bytes(b, sides.base);
            let ow = whole_bytes(o, sides.ours);
            let tw = whole_bytes(t, sides.theirs);
            if ow == bw {
                (Some(tw.to_vec()), 0)
            } else if tw == bw || ow == tw {
                (Some(ow.to_vec()), 0)
            } else {
                merge_container_3way(sides, moved_out, depth, b, o, t, markers)
            }
        }
    }
}

/// Merge a container that genuinely diverged across sides *against a base
/// anchor*: 3-way merge its header text, recurse [`merge_region`] over its body
/// children (base body vs ours vs theirs), 3-way merge its footer text, and
/// concatenate. Entered ONLY for the base-anchored 3-way-modify case — a real
/// base container exists to diff each side against, which is what makes the
/// recursive body merge sound. The no-base add/add case never reaches here: it
/// is resolved by a whole-container comparison in [`resolve_container`] (no
/// recursion without an anchor — heddle#490 r5).
fn merge_container_3way(
    sides: SideSources<'_>,
    moved_out: &MovedOutMethods<'_>,
    depth: usize,
    base: &Item,
    ours: &Item,
    theirs: &Item,
    markers: ConflictMarkers<'_>,
) -> (Option<Vec<u8>>, usize) {
    // Precondition (guaranteed by `resolve_node`'s `all_containers` gate): every
    // participating side is a container WITH a body. The `header_bytes` /
    // `footer_bytes` / `body.as_ref()` reads below depend on it; a mixed
    // container/leaf key never reaches here (it routes to whole-item text merge).
    debug_assert!(
        base.body.is_some() && ours.body.is_some() && theirs.body.is_some(),
        "merge_container_3way entered with a leaf side — structural precondition violated"
    );
    let (header, hc) = merge3_text(
        header_bytes(base, sides.base),
        header_bytes(ours, sides.ours),
        header_bytes(theirs, sides.theirs),
        markers,
    );

    let bb = base.body.as_ref().expect("container");
    let ob = ours.body.as_ref().expect("container");
    let tb = theirs.body.as_ref().expect("container");

    // The body's STRUCTURAL opening/closing delimiters (`{` / `}` for brace
    // languages; empty for delimiter-less Python `block`s) are merged ONCE
    // here and emitted around the woven children — never folded into the child
    // weave. Folding them in let an empty base body, whose only inter-item
    // range is the whole `{}`, re-emit each side's opening `{` in that side's
    // first added-child slot: `mod foo {}` + ours adds `fn a` + theirs adds
    // `fn b` produced two `{` (heddle#490 r6). With the delimiters peeled off,
    // `merge_region` only ever weaves the inter-child content between
    // `content_start` and `content_end`, so the opening delimiter is emitted
    // exactly once for the region.
    let (open, oc) = merge3_text(
        &sides.base.as_bytes()[bb.inner_start..bb.content_start],
        &sides.ours.as_bytes()[ob.inner_start..ob.content_start],
        &sides.theirs.as_bytes()[tb.inner_start..tb.content_start],
        markers,
    );
    let (body, bc) = merge_region(
        sides,
        moved_out,
        depth + 1,
        &bb.items,
        &ob.items,
        &tb.items,
        (bb.content_start, bb.content_end),
        (ob.content_start, ob.content_end),
        (tb.content_start, tb.content_end),
        markers,
    );
    let (close, cc) = merge3_text(
        &sides.base.as_bytes()[bb.content_end..bb.inner_end],
        &sides.ours.as_bytes()[ob.content_end..ob.inner_end],
        &sides.theirs.as_bytes()[tb.content_end..tb.inner_end],
        markers,
    );

    let (footer, fc) = merge3_text(
        footer_bytes(base, sides.base),
        footer_bytes(ours, sides.ours),
        footer_bytes(theirs, sides.theirs),
        markers,
    );

    let mut out = header;
    out.extend_from_slice(&open);
    out.extend_from_slice(&body);
    out.extend_from_slice(&close);
    out.extend_from_slice(&footer);
    (Some(out), hc + oc + bc + cc + fc)
}

/// 3-way merge a slice of bytes (a base-anchored container header/footer).
/// Equal-sides dedup and unchanged-side defer short-circuit; otherwise fall
/// through to the text hunk merge. Only ever called with a real base anchor —
/// the no-base add/add case is conflicted as a whole container upstream in
/// [`resolve_container`] and never recurses into header/footer here.
fn merge3_text(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    markers: ConflictMarkers<'_>,
) -> (Vec<u8>, usize) {
    if ours == theirs {
        return (ours.to_vec(), 0);
    }
    if ours == base {
        return (theirs.to_vec(), 0);
    }
    if theirs == base {
        return (ours.to_vec(), 0);
    }
    materialize_segment(
        text_hunk_merge_with_markers(base, ours, theirs, markers),
        std::str::from_utf8(base).unwrap_or(""),
    )
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
        Some(emit_addadd_conflict(
            &ours_bytes,
            &theirs_bytes,
            markers,
            sides,
        )),
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
