// SPDX-License-Identifier: Apache-2.0
//! Slide Myers equals the way similar's Compact hook does.
//!
//! Left-slide absorbs a preceding insert (`b/a` vs `a/a` → `(1,0)`).
//! Right-slide absorbs a trailing insert (`b/a` vs `a/b/b` → `(0,2)`).

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

/// Slide a trailing equal onto later new-side inserts, matching similar's
/// Compact insert shift-up. `b\na` vs `a\nb\nb` becomes `(0, 2)`, not `(0, 1)`.
///
/// `gap_start` is the exclusive end of the original (pre-left-slide) run so a
/// left-slide is not undone. A partial suffix slide splits the run.
pub(super) fn slide_equal_run_right(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    gap_start: usize,
    gap_end: usize,
) -> (Option<EqualRun>, EqualRun) {
    if run.len == 0 || gap_end <= gap_start {
        return (None, run);
    }
    let max_slide = run.len.min(gap_end - gap_start);
    let mut slide = 0usize;
    while slide < max_slide
        && line_bytes(old.bytes, old.offs[run.old_start + run.len - 1 - slide])
            == line_bytes(new.bytes, new.offs[gap_end - 1 - slide])
    {
        slide += 1;
    }
    if slide == 0 {
        (None, run)
    } else if slide == run.len {
        (
            None,
            EqualRun {
                old_start: run.old_start,
                new_start: gap_end - slide,
                len: slide,
            },
        )
    } else {
        (
            Some(EqualRun {
                old_start: run.old_start,
                new_start: run.new_start,
                len: run.len - slide,
            }),
            EqualRun {
                old_start: run.old_start + run.len - slide,
                new_start: gap_end - slide,
                len: slide,
            },
        )
    }
}

/// True when a later new-side insert can absorb the last line of `run`.
pub(super) fn trailing_insert_can_take_equal(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    original_end: usize,
) -> bool {
    run.len > 0
        && original_end < new.offs.len()
        && line_bytes(old.bytes, old.offs[run.old_start + run.len - 1])
            == line_bytes(new.bytes, new.offs[original_end])
}
