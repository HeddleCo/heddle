// SPDX-License-Identifier: Apache-2.0
//! Visit Myers equals after Compact left/right slides.

use super::compact::{compact_keeps_right_shift, slide_equal_run, slide_equal_run_right};
use super::myers::LineView;
use super::{EqualRun, LineDiffError};

pub(super) struct EqualEmit<'a> {
    pub new_cursor: &'a mut usize,
}

pub(super) fn emit_slid<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    emit: &mut EqualEmit<'_>,
    visit: &mut impl FnMut(EqualRun) -> Result<(), E>,
) -> Result<(), LineDiffError<E>> {
    let original_end = run.new_start + run.len;
    let cursor_before = *emit.new_cursor;
    let (first, second) = slide_equal_run(old, new, run, emit.new_cursor);
    match second {
        Some(second) => {
            visit(first).map_err(LineDiffError::Visitor)?;
            visit_after_right_slide(
                old,
                new,
                second,
                original_end,
                second.new_start <= cursor_before,
                visit,
            )
        }
        None => visit_after_right_slide(
            old,
            new,
            first,
            original_end,
            first.new_start <= cursor_before,
            visit,
        ),
    }
}

fn visit_after_right_slide<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    original_end: usize,
    left_justified: bool,
    visit: &mut impl FnMut(EqualRun) -> Result<(), E>,
) -> Result<(), LineDiffError<E>> {
    let run = if compact_keeps_right_shift(old, new, run, original_end, left_justified) {
        slide_equal_run_right(old, new, run, original_end, new.offs.len(), left_justified).1
    } else {
        run
    };
    visit(run).map_err(LineDiffError::Visitor)
}
