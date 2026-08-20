// SPDX-License-Identifier: Apache-2.0
//! Slide Myers equals the way similar's Compact hook does.

use super::myers::LineView;
use super::scan::line_bytes;
use super::EqualRun;

/// Move an equal run onto a preceding new-side insert when the insert
/// prefix matches the equal's old lines. `b\na` vs `a\na` becomes (1,0),
/// matching `similar::capture_diff_slices`.
pub(super) fn slide_equals_left(old: LineView<'_>, new: LineView<'_>, runs: &mut [EqualRun]) {
    let mut new_cursor = 0usize;
    for run in runs.iter_mut() {
        let insert_len = run.new_start.saturating_sub(new_cursor);
        let mut slide = 0usize;
        let max_slide = insert_len.min(run.len);
        while slide < max_slide
            && line_bytes(old.bytes, old.offs[run.old_start + slide])
                == line_bytes(new.bytes, new.offs[new_cursor + slide])
        {
            slide += 1;
        }
        if slide > 0 {
            run.new_start = new_cursor;
        }
        new_cursor = run.new_start + run.len;
    }
}
