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

use super::items::{FileSegments, Item, ItemKey};

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
type MatchKey = (ItemKey, usize);

/// Stitch three sides together via per-item resolution + inter-item hunk merge.
pub(crate) fn reconstruct_merged_file(
    base: &str,
    ours: &str,
    theirs: &str,
    base_segments: &FileSegments,
    ours_segments: &FileSegments,
    theirs_segments: &FileSegments,
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

    for key in &all_keys {
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

    let item_emit_order = compute_item_emit_order(&base_mks, &ours_mks, &theirs_mks, &all_keys);

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

    // Per-side set of inter-item range indices already emitted into
    // `output`. A side's range is contributed to at most one slot —
    // either the per-item preceding-segment merge for an item the
    // side has, the bridging slot for an item the side lacks (next
    // item the side has owns this range too — so the first occupant
    // wins), or the postamble. Without this tracking the same range
    // can be pulled into multiple slots — both Codex r2 P2 #2
    // (leading-added-item preamble duplication) and P1 #2 (zero-items
    // side postamble duplication) are this single shape.
    let mut emitted: [BTreeSet<usize>; N_SIDES] =
        [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()];

    // Walk emit_order. For each item, emit:
    //   1. The inter-item segment that PRECEDED it in each side. When
    //      the side has the item, that's the side's range immediately
    //      before; when it doesn't, the "bridging" range (the range in
    //      the side that spans where this item would be in emit order)
    //      stands in. Either way, a range is only contributed if it
    //      hasn't already been emitted on a prior iteration.
    //   2. The merged item bytes.
    // After the last item, emit the postamble (each side's final range,
    // skipped per-side if already consumed).
    let mut output: Vec<u8> = Vec::new();

    for (emit_idx, key) in item_emit_order.iter().enumerate() {
        let mut segs: [Option<&str>; N_SIDES] = [None, None, None];
        for s in 0..N_SIDES {
            let r = side_range_for_emit(&side_idx_maps[s], key, &item_emit_order, emit_idx);
            if emitted[s].insert(r) {
                segs[s] = Some(inter_slice(side_sources[s], &side_ranges[s], r));
            }
        }
        let (seg_bytes, seg_conflicts) = merge_segment(segs[0], segs[1], segs[2], markers);
        output.extend_from_slice(&seg_bytes);
        total_conflicts += seg_conflicts;

        if let Some((Some(item_bytes), _)) = resolved.get(key) {
            output.extend_from_slice(item_bytes);
        }
    }

    // Postamble: each side's last range, but only if that range
    // wasn't already pulled in as a bridge above (the zero-items-side
    // shape from P1 #2).
    let mut post: [Option<&str>; N_SIDES] = [None, None, None];
    for s in 0..N_SIDES {
        let last = side_ranges[s].len() - 1;
        if emitted[s].insert(last) {
            post[s] = Some(inter_slice(side_sources[s], &side_ranges[s], last));
        }
    }
    let (post_bytes, post_conflicts) = merge_segment(post[0], post[1], post[2], markers);
    // Only emit the postamble if it adds bytes — otherwise we risk
    // duplicating the trailing newline already in the last item's bytes.
    if !post_bytes.is_empty() {
        output.extend_from_slice(&post_bytes);
    }
    total_conflicts += post_conflicts;

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

/// Pick the inter-item range index that represents `key`'s preceding
/// segment on one side. If the side has `key`, that's the range
/// immediately before it. If not, the bridging range is used: the
/// range in the side that spans `key`'s position in `emit_order`. The
/// bridging range is found by walking left in `emit_order` to the
/// nearest prior key the side does have, then taking the range after
/// that key's item; if no prior key exists, the side's preamble
/// (range 0) bridges.
fn side_range_for_emit(
    side_idx_map: &BTreeMap<MatchKey, usize>,
    key: &MatchKey,
    emit_order: &[MatchKey],
    emit_idx: usize,
) -> usize {
    if let Some(i) = side_idx_map.get(key) {
        return *i;
    }
    for j in (0..emit_idx).rev() {
        if let Some(i) = side_idx_map.get(&emit_order[j]) {
            return i + 1;
        }
    }
    0
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
            if o == t || use_items_import_identically(ours_item, theirs_item) {
                // Byte-identical, OR (for `use` items) the same normalizable
                // leaf set with the same visibility spelled differently
                // (`use a::{B}` vs `use a::B`) — one import, two spellings.
                // Dedup to ours rather than conflicting on cosmetic
                // bracketing. Divergent visibility (`pub use` vs `use`) or
                // a non-identical leaf set fails the check and conflicts
                // below; un-normalizable forms (glob / alias) only dedup on
                // exact bytes, so `use a::*` vs `use b::*` still conflicts.
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

/// Whether two add/add `use` items import exactly the same thing — the
/// same normalizable leaf SET with the same visibility — and so should
/// dedup to one line instead of conflicting on cosmetic spelling
/// (`use a::{B}` vs `use a::B`). Returns `false` unless BOTH items are
/// normalizable `use` items: divergent visibility (`pub use` vs `use`),
/// a differing leaf set, or any un-normalizable form (glob / alias /
/// nested group) falls through to the conflict path. Leaf sets are
/// compared order-insensitively; a single declaration never repeats a leaf.
fn use_items_import_identically(ours: Option<&Item>, theirs: Option<&Item>) -> bool {
    let (Some(a), Some(b)) = (
        ours.and_then(|i| i.use_identity.as_ref()),
        theirs.and_then(|i| i.use_identity.as_ref()),
    ) else {
        return false;
    };
    if !a.normalizable || !b.normalizable || a.visibility != b.visibility {
        return false;
    }
    let mut ours_leaves = a.leaves.clone();
    let mut theirs_leaves = b.leaves.clone();
    ours_leaves.sort();
    theirs_leaves.sort();
    ours_leaves == theirs_leaves
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
