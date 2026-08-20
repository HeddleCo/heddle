// SPDX-License-Identifier: Apache-2.0
//! Public visitor entry for scratch-budgeted equal-run LCS.

use super::super::budget::{ResourceBudget, ResourceKind, ResourceUsage};
use super::myers::{LineView, emit_equal_runs};
use super::scan::{LineOff, count_text_lines, fill_line_offsets};
use super::scratch::{ConquerJob, layout_sizes, require_scratch};
use super::{EqualRun, LcsVisitResult, LineDiffError, LineDiffLimits};

/// Visit equal index ranges in deterministic Myers order.
///
/// Line scanning writes offsets into `scratch`. The algorithm never builds
/// `Vec<String>` inputs or a `Vec<(usize, usize)>` match set. Work exhaustion
/// is [`super::LineDiffError::BudgetExceeded`]. A visitor `Err` stops promptly.
pub fn visit_lcs_equal_runs<E>(
    old_bytes: &[u8],
    new_bytes: &[u8],
    scratch: &mut [u8],
    limits: LineDiffLimits,
    visit: impl FnMut(EqualRun) -> Result<(), E>,
) -> LcsVisitResult<E> {
    let old_lines = count_text_lines(old_bytes).map_err(|_| LineDiffError::InvalidUtf8)?;
    let new_lines = count_text_lines(new_bytes).map_err(|_| LineDiffError::InvalidUtf8)?;

    let scratch_limit = (limits.scratch_bytes as usize).min(scratch.len());
    let mut budget = ResourceBudget::new(ResourceUsage {
        scratch_bytes: scratch_limit as u64,
        lines: limits.max_lines,
        work: limits.max_work,
        states: u64::MAX,
        decoded_bytes: u64::MAX,
    });

    budget.require(ResourceKind::Lines, old_lines as u64)?;
    budget.require(ResourceKind::Lines, new_lines as u64)?;
    budget.record(ResourceKind::Lines, old_lines.max(new_lines) as u64);

    let pair_work = (old_lines as u64).saturating_mul(new_lines as u64);
    budget.require(ResourceKind::Work, pair_work)?;

    let (needed, layout) = layout_sizes(old_lines, new_lines);
    require_scratch(scratch_limit, needed)?;
    let scratch = &mut scratch[..scratch_limit];
    scratch[..needed].fill(0);
    budget.record(ResourceKind::ScratchBytes, needed as u64);

    let parts = unsafe { partition(scratch, &layout) };
    let filled_old =
        fill_line_offsets(old_bytes, parts.old_offs).map_err(|_| LineDiffError::InvalidUtf8)?;
    let filled_new =
        fill_line_offsets(new_bytes, parts.new_offs).map_err(|_| LineDiffError::InvalidUtf8)?;
    if filled_old != old_lines || filled_new != new_lines {
        return Err(LineDiffError::InvalidUtf8);
    }

    emit_equal_runs(
        LineView {
            bytes: old_bytes,
            offs: parts.old_offs,
        },
        LineView {
            bytes: new_bytes,
            offs: parts.new_offs,
        },
        parts.vf,
        parts.vb,
        parts.jobs,
        &mut budget,
        visit,
    )?;
    Ok(budget.used())
}

struct ScratchParts<'a> {
    old_offs: &'a mut [LineOff],
    new_offs: &'a mut [LineOff],
    vf: &'a mut [usize],
    vb: &'a mut [usize],
    jobs: &'a mut [ConquerJob],
}

/// Split caller scratch into disjoint typed regions.
///
/// Safety: `layout` offsets were produced by [`layout_sizes`] and do not overlap
/// inside `scratch`.
unsafe fn partition<'a>(
    scratch: &'a mut [u8],
    layout: &super::scratch::ScratchLayout,
) -> ScratchParts<'a> {
    let base = scratch.as_mut_ptr();
    unsafe {
        ScratchParts {
            old_offs: raw_slice(base, layout.old_off, layout.old_off_bytes),
            new_offs: raw_slice(base, layout.new_off, layout.new_off_bytes),
            vf: raw_slice(base, layout.vf, layout.vf_bytes),
            vb: raw_slice(base, layout.vb, layout.vb_bytes),
            jobs: raw_slice(base, layout.jobs, layout.jobs_bytes),
        }
    }
}

unsafe fn raw_slice<'a, T>(base: *mut u8, start: usize, bytes: usize) -> &'a mut [T] {
    let count = if std::mem::size_of::<T>() == 0 {
        0
    } else {
        bytes / std::mem::size_of::<T>()
    };
    unsafe { std::slice::from_raw_parts_mut(base.add(start).cast::<T>(), count) }
}
