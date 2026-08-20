// SPDX-License-Identifier: Apache-2.0
//! Visit Myers equals after Compact left/right slides.

use super::compact::{compact_keeps_right_shift, slide_equal_run, slide_equal_run_right};
use super::myers::LineView;
use super::{EqualRun, LineDiffError};

pub(super) struct PendingEqual {
    run: EqualRun,
    original_end: usize,
    left_justified: bool,
}

pub(super) struct EqualEmit<'a> {
    pub new_cursor: &'a mut usize,
    pub pending: &'a mut Option<PendingEqual>,
}

pub(super) fn emit_slid<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    emit: &mut EqualEmit<'_>,
    visit: &mut impl FnMut(EqualRun) -> Result<(), E>,
) -> Result<(), LineDiffError<E>> {
    if let Some(prev) = emit.pending.take() {
        visit(prev.run).map_err(LineDiffError::Visitor)?;
    }
    let original_end = run.new_start + run.len;
    let cursor_before = *emit.new_cursor;
    let (first, second) = slide_equal_run(old, new, run, emit.new_cursor);
    match second {
        Some(second) => {
            visit(first).map_err(LineDiffError::Visitor)?;
            defer_or_visit(
                old,
                new,
                second,
                original_end,
                second.new_start <= cursor_before,
                emit.pending,
                visit,
            )
        }
        None => defer_or_visit(
            old,
            new,
            first,
            original_end,
            first.new_start <= cursor_before,
            emit.pending,
            visit,
        ),
    }
}

fn defer_or_visit<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    run: EqualRun,
    original_end: usize,
    left_justified: bool,
    pending: &mut Option<PendingEqual>,
    visit: &mut impl FnMut(EqualRun) -> Result<(), E>,
) -> Result<(), LineDiffError<E>> {
    if compact_keeps_right_shift(old, new, run, original_end, left_justified) {
        *pending = Some(PendingEqual {
            run,
            original_end,
            left_justified,
        });
        Ok(())
    } else {
        visit(run).map_err(LineDiffError::Visitor)
    }
}

pub(super) fn flush_pending<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    pending: Option<PendingEqual>,
    gap_end: usize,
    visit: &mut impl FnMut(EqualRun) -> Result<(), E>,
) -> Result<(), LineDiffError<E>> {
    let Some(pending) = pending else {
        return Ok(());
    };
    let (head, tail) = slide_equal_run_right(
        old,
        new,
        pending.run,
        pending.original_end,
        gap_end,
        pending.left_justified,
    );
    if let Some(head) = head {
        visit(head).map_err(LineDiffError::Visitor)?;
    }
    visit(tail).map_err(LineDiffError::Visitor)
}
