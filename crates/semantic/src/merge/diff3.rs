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
    } else {
        emit_conflict(out, our_slice, their_slice, markers);
        *conflicts += 1;
    }
}

fn slice_eq(a: &[&[u8]], b: &[&[u8]]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}
