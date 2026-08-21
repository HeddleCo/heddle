// SPDX-License-Identifier: Apache-2.0
//! Slide Myers equals the way similar's Compact hook does.
//!
//! Left-slide absorbs a preceding insert (`b/a` vs `a/a` → `(1,0)`).
//! Right-slide absorbs one adjacent trailing insert (`b/a` vs `a/b/b` →
//! `(0,2)`). Compact does not walk every later copy (`a` vs `b/a/a/a/b`
//! stays `(0,2)`). An intervening different line is not a Compact shift
//! (`b/a` vs `a/b/c/b/b` stays `(0,1)`). A left-justified prefix is not
//! moved just because the same line repeats at the end (`a` vs `a/a`
//! stays `(0,0)`).

use super::EqualRun;
use super::myers::LineView;
use super::scan::line_bytes;

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

/// Slide a whole-run equal one adjacent Compact shift-up, or leave it.
///
/// Compact compares the equal to the immediately following insert once, and
/// only keeps that shift when the remaining insert's suffix also matches.
/// It does not walk every later copy (`a` vs `b/a/a/a/b` stays `(0,2)`),
/// and it does not jump onto a later suffix after a different line
/// (`b/a` vs `a/b/c/b/b` stays `(0,1)`). `b/a` vs `a/b/b` slides
/// `(0,1)` → `(0,2)`.
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
    if gap_start + run.len <= gap_end
        && run_matches_at(old, new, run, gap_start)
        && run_matches_at(old, new, run, gap_end - run.len)
    {
        (
            None,
            EqualRun {
                old_start: run.old_start,
                new_start: gap_start,
                len: run.len,
            },
        )
    } else {
        (None, run)
    }
}

/// True when Compact would keep one adjacent right shift of this equal.
///
/// A run already sitting on the unconsumed new cursor is a common prefix of
/// the remaining window. Compact shift-down would undo a right slide.
/// Shift-up also requires the remaining insert's suffix to match; an
/// adjacent copy alone is not enough (`a` vs `b/a/a/a/b` stays `(0,2)`).
/// A later suffix after a different line is not enough either
/// (`b/a` vs `a/b/c/b/b` stays `(0,1)`).
pub(super) fn compact_keeps_right_shift(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    original_end: usize,
    left_justified: bool,
) -> bool {
    if left_justified || run.len == 0 || original_end + run.len > new.offs.len() {
        return false;
    }
    run_matches_at(old, new, run, original_end)
        && run_matches_at(old, new, run, new.offs.len() - run.len)
}

fn run_matches_at(old: LineView<'_>, new: LineView<'_>, run: EqualRun, new_start: usize) -> bool {
    (0..run.len).all(|offset| {
        line_bytes(old.bytes, old.offs[run.old_start + offset])
            == line_bytes(new.bytes, new.offs[new_start + offset])
    })
}
