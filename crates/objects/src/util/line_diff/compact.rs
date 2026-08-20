// SPDX-License-Identifier: Apache-2.0
//! Slide Myers equals the way similar's Compact hook does.
//!
//! Left-slide absorbs a preceding insert (`b/a` vs `a/a` → `(1,0)`).
//! Right-slide absorbs a trailing insert only when Compact would keep the
//! shift (`b/a` vs `a/b/b` → `(0,2)`). A left-justified prefix is not moved
//! just because the same line repeats at the end (`a` vs `a/a` stays `(0,0)`).

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
/// Compact insert shift-up *and* shift-down.
///
/// Compact first shifts an insert up (equal moves right onto a trailing
/// duplicate) then down (equal moves left onto a preceding insert). A
/// left-justified prefix (`a` vs `a/a`) is restored by shift-down, so it
/// must stay `(0, 0)`. `b/a` vs `a/b/b` stays `(0, 2)` because the
/// preceding insert is `a`, which cannot pull the `b` back.
///
/// `gap_start` is the exclusive end of the original (pre-left-slide) run so a
/// left-slide is not undone. Only a whole-run slide is applied.
pub(super) fn slide_equal_run_right(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    gap_start: usize,
    gap_end: usize,
    left_justified: bool,
) -> (Option<EqualRun>, EqualRun) {
    if left_justified || run.len == 0 || gap_end <= gap_start {
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
    if slide == run.len {
        (
            None,
            EqualRun {
                old_start: run.old_start,
                new_start: gap_end - slide,
                len: slide,
            },
        )
    } else {
        (None, run)
    }
}

/// True when Compact would keep a right shift of this equal.
///
/// A run already sitting on the unconsumed new cursor is a common prefix of
/// the remaining window. Compact shift-down would undo a right slide.
pub(super) fn compact_keeps_right_shift(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    original_end: usize,
    left_justified: bool,
) -> bool {
    if left_justified || run.len == 0 || original_end >= new.offs.len() {
        return false;
    }
    let gap = new.offs.len() - original_end;
    if run.len > gap {
        return false;
    }
    (0..run.len).all(|offset| {
        line_bytes(old.bytes, old.offs[run.old_start + offset])
            == line_bytes(new.bytes, new.offs[new.offs.len() - run.len + offset])
    })
}
