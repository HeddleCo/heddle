// SPDX-License-Identifier: Apache-2.0
//! diff3 algorithm: line-level three-way merge with per-hunk conflict markers.
//!
//! Walks `base` once, identifying *stable* lines (lines that align to the
//! current expected position in BOTH `ours` and `theirs`) and *unstable hunks*
//! (everything else). Each unstable hunk is classified one of four ways and
//! emitted accordingly. Total cost is O(n) over base lines plus the LCS cost
//! of computing the two pairwise alignments via `similar::capture_diff_slices`.
//!
//! The classification mirrors git's `merge-file` semantics:
//!
//! | ours == base | theirs == base | ours == theirs | outcome           |
//! |--------------|----------------|----------------|-------------------|
//! | ✓            | —              | —              | take theirs       |
//! | —            | ✓              | —              | take ours         |
//! | —            | —              | ✓              | take either       |
//! | —            | —              | —              | conflict markers  |
//!
//! When the simple four-way classification falls through (so the hunk would
//! otherwise be a conflict), a finer-grained composer in [`compose_disjoint`]
//! attempts to merge a *pure-insertion* side with a *pure-edit* side. That
//! handles the `base = X Y / ours = X NEW Y / theirs = X Y'` shape — git's
//! merge-file gets these right via patch composition, and so do we.

use similar::{Algorithm, DiffOp};

use super::{
    MergeOutcome,
    lines::{build_alignment, split_lines},
    markers::{ConflictMarkers, emit_conflict, emit_lines},
    whitespace::{prefer_clean, trailing_ws_equal},
};

pub(super) fn run(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    // Whole-input shortcuts before paying for diff.
    if base == ours && base == theirs {
        return MergeOutcome::Clean(base.to_vec());
    }
    if base == ours {
        return MergeOutcome::Clean(theirs.to_vec());
    }
    if base == theirs {
        return MergeOutcome::Clean(ours.to_vec());
    }
    if ours == theirs {
        return MergeOutcome::Clean(ours.to_vec());
    }

    let base_lines = split_lines(base);
    let our_lines = split_lines(ours);
    let their_lines = split_lines(theirs);

    let our_align = build_alignment(&base_lines, &our_lines);
    let their_align = build_alignment(&base_lines, &their_lines);

    walk_and_emit(
        &base_lines,
        &our_lines,
        &their_lines,
        &our_align,
        &their_align,
        markers,
    )
}

fn walk_and_emit(
    base: &[&[u8]],
    ours: &[&[u8]],
    theirs: &[&[u8]],
    our_align: &[Option<usize>],
    their_align: &[Option<usize>],
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    let mut output = Vec::new();
    let mut conflicts = 0usize;
    let mut i = 0usize;
    let mut our_pos = 0usize;
    let mut their_pos = 0usize;

    while i < base.len() {
        // Stable line: base[i] aligns with the current our_pos / their_pos.
        if our_align[i] == Some(our_pos) && their_align[i] == Some(their_pos) {
            output.extend_from_slice(base[i]);
            i += 1;
            our_pos += 1;
            their_pos += 1;
            continue;
        }

        // Unstable hunk: scan forward until both sides re-align (or end-of-base).
        let hunk_start = i;
        let mut j = i;
        let (end_our, end_their) = loop {
            if j >= base.len() {
                break (ours.len(), theirs.len());
            }
            if let (Some(o), Some(t)) = (our_align[j], their_align[j])
                && o >= our_pos
                && t >= their_pos
            {
                break (o, t);
            }
            j += 1;
        };

        emit_hunk(
            &mut output,
            &mut conflicts,
            &base[hunk_start..j],
            &ours[our_pos..end_our],
            &theirs[their_pos..end_their],
            markers,
        );

        i = j;
        our_pos = end_our;
        their_pos = end_their;
    }

    // Trailing content past end of base.
    if our_pos < ours.len() || their_pos < theirs.len() {
        emit_hunk(
            &mut output,
            &mut conflicts,
            &[],
            &ours[our_pos..],
            &theirs[their_pos..],
            markers,
        );
    }

    if conflicts == 0 {
        MergeOutcome::Clean(output)
    } else {
        MergeOutcome::Conflicts {
            merged_bytes_with_markers: output,
            conflict_count: conflicts,
        }
    }
}

fn emit_hunk(
    out: &mut Vec<u8>,
    conflicts: &mut usize,
    base_slice: &[&[u8]],
    our_slice: &[&[u8]],
    their_slice: &[&[u8]],
    markers: ConflictMarkers<'_>,
) {
    if slice_eq(our_slice, base_slice) {
        emit_lines(out, their_slice);
    } else if slice_eq(their_slice, base_slice) {
        emit_lines(out, our_slice);
    } else if slice_eq(our_slice, their_slice) || trailing_ws_equal(our_slice, their_slice) {
        emit_lines(out, prefer_clean(our_slice, their_slice));
    } else if let Some(composed) = compose_disjoint(base_slice, our_slice, their_slice) {
        // One side inserted at an anchor and the other edited adjacent base
        // lines — patches compose, no overlap. Without this branch we'd
        // declare a conflict for cases git resolves cleanly.
        out.extend_from_slice(&composed);
    } else if base_slice.is_empty() && !our_slice.is_empty() && !their_slice.is_empty() {
        // Both sides inserted different content at the same anchor point
        // (no base lines consumed by this hunk). heddle's UX choice — also
        // the prior single-range merger's behavior — is to concatenate
        // both insertions rather than emit a conflict, which preserves
        // the common parallel-thread append flow (e.g. two workers each
        // append a new function to the same file). Order is ours then
        // theirs, matching the prior implementation.
        emit_lines(out, our_slice);
        emit_lines(out, their_slice);
    } else {
        emit_conflict(out, our_slice, their_slice, markers);
        *conflicts += 1;
    }
}

fn slice_eq(a: &[&[u8]], b: &[&[u8]]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

/// Classification of what a side does to a single base line.
enum LineAction<'a> {
    /// Base line is preserved verbatim.
    Keep,
    /// Base line replaced with the given replacement lines (possibly more
    /// or fewer than one).
    Replace(Vec<&'a [u8]>),
    /// Base line dropped.
    Delete,
}

/// Try to merge a hunk where one side ONLY inserts (between or around base
/// lines) and the other side ONLY edits (replaces / deletes base lines)
/// without overlapping the insertions. Returns the composed bytes if the two
/// patches commute; `None` otherwise (caller should emit a conflict).
///
/// Both orderings (ours = insertion / theirs = edit, and vice versa) are
/// tried so the caller doesn't have to. The composer is a strict commute
/// check: any base line modified by both sides — or any same-anchor
/// insertion with different content — bails out so the conflict marker
/// path remains correct.
fn compose_disjoint(base: &[&[u8]], ours: &[&[u8]], theirs: &[&[u8]]) -> Option<Vec<u8>> {
    let (our_actions, our_inserts) = classify_against_base(base, ours);
    let (their_actions, their_inserts) = classify_against_base(base, theirs);

    let mut out = Vec::new();
    for i in 0..base.len() {
        // Gap-i insertions (before base[i]).
        compose_gap(&mut out, &our_inserts[i], &their_inserts[i])?;
        // Action on base[i].
        match (&our_actions[i], &their_actions[i]) {
            (LineAction::Keep, LineAction::Keep) => out.extend_from_slice(base[i]),
            (LineAction::Keep, LineAction::Replace(repl))
            | (LineAction::Replace(repl), LineAction::Keep) => {
                for line in repl {
                    out.extend_from_slice(line);
                }
            }
            (LineAction::Keep, LineAction::Delete) | (LineAction::Delete, LineAction::Keep) => {}
            // Both sides modify the same base line in any way — bail.
            (LineAction::Replace(_), LineAction::Replace(_))
            | (LineAction::Replace(_), LineAction::Delete)
            | (LineAction::Delete, LineAction::Replace(_))
            | (LineAction::Delete, LineAction::Delete) => return None,
        }
    }
    // Trailing-gap insertions (past end of base).
    compose_gap(
        &mut out,
        &our_inserts[base.len()],
        &their_inserts[base.len()],
    )?;
    Some(out)
}

/// Build (per-base-line action, per-gap insertion) tables from one side's
/// LCS diff against `base`. `inserts[k]` collects lines inserted in the gap
/// before `base[k]`; `inserts[base.len()]` collects post-`base` insertions.
fn classify_against_base<'a>(
    base: &[&[u8]],
    side: &'a [&'a [u8]],
) -> (Vec<LineAction<'a>>, Vec<Vec<&'a [u8]>>) {
    let mut actions: Vec<LineAction<'a>> = (0..base.len()).map(|_| LineAction::Keep).collect();
    let mut inserts: Vec<Vec<&'a [u8]>> = vec![Vec::new(); base.len() + 1];
    let ops = similar::capture_diff_slices(Algorithm::Histogram, base, side);
    for op in ops {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                let slot = old_index.min(base.len());
                for k in 0..new_len {
                    inserts[slot].push(side[new_index + k]);
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for k in 0..old_len {
                    actions[old_index + k] = LineAction::Delete;
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let mut repl: Vec<&[u8]> = Vec::with_capacity(new_len);
                for k in 0..new_len {
                    repl.push(side[new_index + k]);
                }
                actions[old_index] = LineAction::Replace(repl);
                for k in 1..old_len {
                    actions[old_index + k] = LineAction::Delete;
                }
            }
        }
    }
    (actions, inserts)
}

/// Compose two same-anchor insertion lists into the output. If only one side
/// inserts, emit it. If both insert the same lines, emit once. If both
/// insert *different* lines at the same anchor, the composer cannot prove
/// commutation — return `None` so the caller falls through to a conflict.
fn compose_gap(out: &mut Vec<u8>, ours: &[&[u8]], theirs: &[&[u8]]) -> Option<()> {
    if ours.is_empty() && theirs.is_empty() {
        return Some(());
    }
    if ours.is_empty() {
        for line in theirs {
            out.extend_from_slice(line);
        }
        return Some(());
    }
    if theirs.is_empty() {
        for line in ours {
            out.extend_from_slice(line);
        }
        return Some(());
    }
    if ours.len() == theirs.len() && ours.iter().zip(theirs).all(|(a, b)| a == b) {
        for line in ours {
            out.extend_from_slice(line);
        }
        return Some(());
    }
    None
}
