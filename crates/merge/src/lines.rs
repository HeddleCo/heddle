// SPDX-License-Identifier: Apache-2.0
//! Line splitting and base-to-other alignment construction.

use similar::{Algorithm, DiffOp};

/// Split bytes into lines, with each line retaining its trailing `\n` (if any).
///
/// `b"a\nb\n"` → `[b"a\n", b"b\n"]` (two lines).
/// `b"a\nb"`   → `[b"a\n", b"b"]`  (final line has no newline; preserved).
/// `b""`        → `[]`              (no lines).
pub(super) fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    bytes.split_inclusive(|&b| b == b'\n').collect()
}

/// For each base-line index, record the matching index in `other` (if any).
///
/// Built from `similar::Algorithm::Histogram` Equal ops, which give exact
/// position-pairs `(old_index + k, new_index + k)` for k in 0..len. Indices
/// not covered by any Equal op are `None` — that base line has no
/// counterpart in `other` (it was modified or deleted on that side).
pub(super) fn build_alignment(base_lines: &[&[u8]], other_lines: &[&[u8]]) -> Vec<Option<usize>> {
    let mut align = vec![None; base_lines.len()];
    let ops = similar::capture_diff_slices(Algorithm::Histogram, base_lines, other_lines);
    for op in ops {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            for k in 0..len {
                align[old_index + k] = Some(new_index + k);
            }
        }
    }
    align
}
