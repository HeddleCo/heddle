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
//!    one-side-only added items appended at their natural position relative
//!    to neighbours.
//! 3. Running `heddle-merge::text_hunk_merge` on the *concatenated inter-item
//!    content* to resolve preamble / between / postamble edits.

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

    // Build the merged file. Strategy:
    //
    //   1. Emit the merged *inter-item content* by concatenating each side's
    //      inter-item slices and running text_hunk_merge once. The result is
    //      a single byte string of "non-item" content.
    //   2. Emit each resolved item in base order. Items that don't exist in
    //      base (added on ours or theirs) get appended in their respective
    //      side's relative position to the nearest matched neighbour.
    //
    // For the inter-item merge we use the simple concatenation trick because
    // segment boundaries can shift when items are reordered; aligning per-
    // segment would require a separate matching pass.
    let inter_outcome = merge_inter_item_content(
        base,
        ours,
        theirs,
        base_segments,
        ours_segments,
        theirs_segments,
        markers,
    );
    let (inter_bytes, inter_conflicts) = match inter_outcome {
        MergeOutcome::Clean(bytes) => (bytes, 0),
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            conflict_count,
        } => (merged_bytes_with_markers, conflict_count),
        // Inter-item content is never binary on its own — it's a subset of
        // text we already know parses. But carry the contract for safety.
        MergeOutcome::Binary | MergeOutcome::DeleteVsModify => (
            inter_content_concat(base, base_segments).into_bytes(),
            0,
        ),
    };
    total_conflicts += inter_conflicts;

    // For v1: emit the inter-item bytes first, then all the resolved items
    // separated by a single newline. This loses fidelity on inter-item
    // placement between specific items, but it produces a coherent, valid
    // file. The trade-off is documented in `docs/design/semantic-merge-function-level.md`.
    //
    // A more sophisticated reconstruction would weave inter-item bytes
    // between items in source order — that's the right v2.
    //
    // For *most* real codebases the inter-item content is "use statements
    // and a doc comment at the top of the file", which concatenates cleanly.
    let mut output = inter_bytes;
    ensure_trailing_newline(&mut output);

    let item_emit_order = compute_item_emit_order(
        base_segments,
        ours_segments,
        theirs_segments,
        &all_keys,
    );

    for key in item_emit_order {
        if let Some((Some(bytes), _)) = resolved.get(&key) {
            output.extend_from_slice(bytes);
            ensure_trailing_newline(&mut output);
        }
    }

    if total_conflicts == 0 {
        MergeOutcome::Clean(output)
    } else {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers: output,
            conflict_count: total_conflicts,
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

fn merge_inter_item_content(
    base: &str,
    ours: &str,
    theirs: &str,
    base_segments: &FileSegments,
    ours_segments: &FileSegments,
    theirs_segments: &FileSegments,
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    let base_concat = inter_content_concat(base, base_segments);
    let ours_concat = inter_content_concat(ours, ours_segments);
    let theirs_concat = inter_content_concat(theirs, theirs_segments);
    text_hunk_merge_with_markers(
        base_concat.as_bytes(),
        ours_concat.as_bytes(),
        theirs_concat.as_bytes(),
        markers,
    )
}

fn inter_content_concat(source: &str, segments: &FileSegments) -> String {
    let mut out = String::new();
    for (start, end) in segments.inter_item_ranges() {
        out.push_str(&source[start..end]);
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
    base_segments: &FileSegments,
    ours_segments: &FileSegments,
    theirs_segments: &FileSegments,
    all_keys: &BTreeSet<&ItemKey>,
) -> Vec<ItemKey> {
    let base_keys: Vec<ItemKey> = base_segments.items.iter().map(|i| i.key.clone()).collect();
    let base_key_set: BTreeSet<&ItemKey> = base_segments.items.iter().map(|i| &i.key).collect();

    let mut order: Vec<ItemKey> = base_keys.clone();
    let mut placed: BTreeSet<ItemKey> = base_keys.iter().cloned().collect();

    // Helper: find a base anchor to splice an added key after, by walking
    // left through `side_keys` until we hit a base key.
    let find_anchor = |side_keys: &[ItemKey], idx: usize| -> Option<ItemKey> {
        for i in (0..idx).rev() {
            if base_key_set.contains(&side_keys[i]) {
                return Some(side_keys[i].clone());
            }
        }
        None
    };

    let splice_added = |order: &mut Vec<ItemKey>,
                        placed: &mut BTreeSet<ItemKey>,
                        side_segments: &FileSegments| {
        let side_keys: Vec<ItemKey> =
            side_segments.items.iter().map(|i| i.key.clone()).collect();
        for (idx, key) in side_keys.iter().enumerate() {
            if placed.contains(key) {
                continue;
            }
            let anchor = find_anchor(&side_keys, idx);
            let insert_at = match anchor {
                Some(anchor_key) => order
                    .iter()
                    .position(|k| *k == anchor_key)
                    .map(|p| p + 1)
                    .unwrap_or(order.len()),
                None => 0,
            };
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
