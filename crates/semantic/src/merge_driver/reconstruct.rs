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

use super::items::{FileSegments, Item, ItemKey};

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
    // Build (key -> item) maps per side for matching.
    let base_map: BTreeMap<ItemKey, &Item> =
        base_segments.items.iter().map(|i| (i.key.clone(), i)).collect();
    let ours_map: BTreeMap<ItemKey, &Item> =
        ours_segments.items.iter().map(|i| (i.key.clone(), i)).collect();
    let theirs_map: BTreeMap<ItemKey, &Item> =
        theirs_segments.items.iter().map(|i| (i.key.clone(), i)).collect();

    let all_keys: BTreeSet<&ItemKey> = base_map
        .keys()
        .chain(ours_map.keys())
        .chain(theirs_map.keys())
        .collect();

    // Resolve every key independently. Each resolution yields either
    // (Some(merged_bytes), conflict_count) or `None` if both sides removed
    // the item.
    let mut resolved: BTreeMap<ItemKey, (Option<Vec<u8>>, usize)> = BTreeMap::new();
    let mut total_conflicts = 0usize;

    for key in &all_keys {
        let resolution = resolve_item(
            base,
            ours,
            theirs,
            base_map.get(*key).copied(),
            ours_map.get(*key).copied(),
            theirs_map.get(*key).copied(),
            markers,
        );
        total_conflicts += resolution.1;
        resolved.insert((*key).clone(), resolution);
    }

    let item_emit_order = compute_item_emit_order(
        base_segments,
        ours_segments,
        theirs_segments,
        &all_keys,
    );

    // For each side, record each item's index so we can look up the
    // inter-item segment that preceded it in source.
    let base_idx = item_index(base_segments);
    let ours_idx = item_index(ours_segments);
    let theirs_idx = item_index(theirs_segments);

    let base_ranges = base_segments.inter_item_ranges();
    let ours_ranges = ours_segments.inter_item_ranges();
    let theirs_ranges = theirs_segments.inter_item_ranges();

    // Walk emit_order. For each item, emit:
    //   1. The inter-item segment that PRECEDED it in the side(s) that
    //      have it. When the item is in base, 3-way merge the preceding
    //      segments from base/ours/theirs; when added on one side, take
    //      that side's preceding segment. This keeps top-level
    //      executable statements (Python `x.init()`, Rust attributes)
    //      at their original source position.
    //   2. The merged item bytes.
    // After the last item, emit the postamble (the trailing inter-item
    // segment on each side that has the last item).
    let mut output: Vec<u8> = Vec::new();

    for (emit_idx, key) in item_emit_order.iter().enumerate() {
        let preceding = (
            base_idx
                .get(key)
                .map(|i| inter_slice(base, &base_ranges, *i)),
            ours_idx
                .get(key)
                .map(|i| inter_slice(ours, &ours_ranges, *i)),
            theirs_idx
                .get(key)
                .map(|i| inter_slice(theirs, &theirs_ranges, *i)),
        );
        // First item: any side that has it contributes its preamble. If
        // none of the sides have this item (shouldn't happen — all_keys
        // is keyed on at least one side), fall through to base preamble.
        let segment = if emit_idx == 0 {
            select_preamble(
                preceding.0,
                preceding.1,
                preceding.2,
                &base_ranges,
                &ours_ranges,
                &theirs_ranges,
                base,
                ours,
                theirs,
            )
        } else {
            preceding
        };
        let (seg_bytes, seg_conflicts) =
            merge_segment(segment.0, segment.1, segment.2, markers);
        output.extend_from_slice(&seg_bytes);
        total_conflicts += seg_conflicts;

        if let Some((Some(item_bytes), _)) = resolved.get(key) {
            output.extend_from_slice(item_bytes);
        }
    }

    // Postamble: the LAST inter-item segment on each side. We pick the
    // postambles unconditionally rather than tying them to the final
    // emitted item — even sides that don't contribute the final item
    // may have a meaningful trailing newline / comment.
    let post = (
        last_segment(base, &base_ranges),
        last_segment(ours, &ours_ranges),
        last_segment(theirs, &theirs_ranges),
    );
    let (post_bytes, post_conflicts) =
        merge_segment(post.0, post.1, post.2, markers);
    // Only emit the postamble if it adds bytes — otherwise we risk
    // duplicating the trailing newline already in the last item's bytes.
    if !post_bytes.is_empty() {
        output.extend_from_slice(&post_bytes);
    }
    total_conflicts += post_conflicts;

    ensure_trailing_newline(&mut output);

    if total_conflicts == 0 {
        MergeOutcome::Clean(output)
    } else {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers: output,
            conflict_count: total_conflicts,
        }
    }
}

fn item_index(segments: &FileSegments) -> BTreeMap<ItemKey, usize> {
    segments
        .items
        .iter()
        .enumerate()
        .map(|(i, it)| (it.key.clone(), i))
        .collect()
}

fn inter_slice<'a>(source: &'a str, ranges: &[(usize, usize)], idx: usize) -> &'a str {
    let (start, end) = ranges[idx];
    &source[start..end]
}

fn last_segment<'a>(source: &'a str, ranges: &[(usize, usize)]) -> Option<&'a str> {
    ranges.last().map(|(s, e)| &source[*s..*e])
}

/// First-item preceding-segment selection. When an item is missing on a
/// side, fall back to that side's actual preamble so the 3-way merge can
/// compare like with like (otherwise preamble edits on a side that
/// dropped the first item get silently dropped).
#[allow(clippy::too_many_arguments)]
fn select_preamble<'a>(
    base_pre: Option<&'a str>,
    ours_pre: Option<&'a str>,
    theirs_pre: Option<&'a str>,
    base_ranges: &[(usize, usize)],
    ours_ranges: &[(usize, usize)],
    theirs_ranges: &[(usize, usize)],
    base: &'a str,
    ours: &'a str,
    theirs: &'a str,
) -> (Option<&'a str>, Option<&'a str>, Option<&'a str>) {
    (
        base_pre.or_else(|| base_ranges.first().map(|(s, e)| &base[*s..*e])),
        ours_pre.or_else(|| ours_ranges.first().map(|(s, e)| &ours[*s..*e])),
        theirs_pre.or_else(|| theirs_ranges.first().map(|(s, e)| &theirs[*s..*e])),
    )
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
        MergeOutcome::Binary | MergeOutcome::DeleteVsModify => {
            (fallback.as_bytes().to_vec(), 0)
        }
    }
}

/// Resolve a single item's 3-way merge. Returns `(Some(bytes), n_conflicts)`
/// when the item survives, `(None, n_conflicts)` when both sides removed
/// it.
fn resolve_item(
    base: &str,
    ours: &str,
    theirs: &str,
    base_item: Option<&Item>,
    ours_item: Option<&Item>,
    theirs_item: Option<&Item>,
    markers: ConflictMarkers<'_>,
) -> (Option<Vec<u8>>, usize) {
    let base_bytes = base_item.map(|i| &base.as_bytes()[i.start_byte..i.end_byte]);
    let ours_bytes = ours_item.map(|i| &ours.as_bytes()[i.start_byte..i.end_byte]);
    let theirs_bytes = theirs_item.map(|i| &theirs.as_bytes()[i.start_byte..i.end_byte]);

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
            if o == t {
                (Some(o.to_vec()), 0)
            } else {
                (Some(emit_addadd_conflict(o, t, markers)), 1)
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
    base_segments: &FileSegments,
    ours_segments: &FileSegments,
    theirs_segments: &FileSegments,
    all_keys: &BTreeSet<&ItemKey>,
) -> Vec<ItemKey> {
    let base_keys: Vec<ItemKey> = base_segments.items.iter().map(|i| i.key.clone()).collect();

    let mut order: Vec<ItemKey> = base_keys.clone();
    let mut placed: BTreeSet<ItemKey> = base_keys.iter().cloned().collect();

    let splice_added = |order: &mut Vec<ItemKey>,
                        placed: &mut BTreeSet<ItemKey>,
                        side_segments: &FileSegments| {
        let side_keys: Vec<ItemKey> =
            side_segments.items.iter().map(|i| i.key.clone()).collect();
        for (idx, key) in side_keys.iter().enumerate() {
            if placed.contains(key) {
                continue;
            }
            // Anchor is the nearest key to the left in this side's source
            // order that's already placed in `order`. That includes both
            // base keys AND earlier-spliced additions from this side —
            // so a run of N adjacent new items splices as a contiguous
            // block preserving source order, rather than each one
            // jumping ahead of its predecessor at the same base anchor.
            let mut insert_at = 0usize;
            for i in (0..idx).rev() {
                if let Some(pos) = order.iter().position(|k| *k == side_keys[i]) {
                    insert_at = pos + 1;
                    break;
                }
            }
            order.insert(insert_at, key.clone());
            placed.insert(key.clone());
        }
    };

    splice_added(&mut order, &mut placed, ours_segments);
    splice_added(&mut order, &mut placed, theirs_segments);

    // Filter to keys that appear in the resolved set (some may have been
    // removed from all sides — those are absent from `all_keys`).
    order
        .into_iter()
        .filter(|k| all_keys.contains(k))
        .collect()
}

fn ensure_trailing_newline(out: &mut Vec<u8>) {
    if !out.is_empty() && *out.last().unwrap() != b'\n' {
        out.push(b'\n');
    }
}

/// Emit a `<<<<<<< / ======= / >>>>>>>` conflict block wrapping two
/// insertion bodies. Mirrors the marker shape `heddle-merge::markers`
/// produces so external validators (heddle#78) and IDE conflict tools
/// parse it identically.
fn emit_addadd_conflict(ours: &[u8], theirs: &[u8], markers: ConflictMarkers<'_>) -> Vec<u8> {
    let mut out = Vec::with_capacity(ours.len() + theirs.len() + 64);
    out.extend_from_slice(b"<<<<<<< ");
    out.extend_from_slice(markers.ours.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(ours);
    ensure_trailing_newline(&mut out);
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(theirs);
    ensure_trailing_newline(&mut out);
    out.extend_from_slice(b">>>>>>> ");
    out.extend_from_slice(markers.theirs.as_bytes());
    out.push(b'\n');
    out
}
