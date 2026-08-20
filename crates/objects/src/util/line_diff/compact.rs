// SPDX-License-Identifier: Apache-2.0
//! Slide Myers equals the way similar's Compact hook does.

use super::myers::LineView;
use super::scan::line_bytes;
use super::EqualRun;

/// Relocate a prefix of `run` onto a preceding new-side insert when that
/// insert matches. A partial slide splits the run; it does not move `len`.
///
/// `b\na` vs `a\na` (`slide == len`) stays `(1,0)`. `x\ny\na\nb` vs
/// `a\nz\na\nb` becomes `(2,0,1)` + `(3,3,1)`, never `(2,0,2)`.
pub(super) fn slide_equal_run(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    new_cursor: &mut usize,
) -> (EqualRun, Option<EqualRun>) {
    let original_new_start = run.new_start;
    let insert_len = original_new_start.saturating_sub(*new_cursor);
    let mut slide = 0usize;
    let max_slide = insert_len.min(run.len);
    while slide < max_slide
        && line_bytes(old.bytes, old.offs[run.old_start + slide])
            == line_bytes(new.bytes, new.offs[*new_cursor + slide])
    {
        slide += 1;
    }
    let slid = if slide == 0 {
        (run, None)
    } else if slide == run.len {
        (
            EqualRun {
                old_start: run.old_start,
                new_start: *new_cursor,
                len: slide,
            },
            None,
        )
    } else {
        (
            EqualRun {
                old_start: run.old_start,
                new_start: *new_cursor,
                len: slide,
            },
            Some(EqualRun {
                old_start: run.old_start + slide,
                new_start: original_new_start + slide,
                len: run.len - slide,
            }),
        )
    };
    *new_cursor = original_new_start + run.len;
    slid
}
