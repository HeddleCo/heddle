// SPDX-License-Identifier: Apache-2.0
//! Public visitor entry for scratch-budgeted equal-run LCS.

use std::mem::align_of;

use super::super::budget::{ResourceBudget, ResourceKind};
use super::myers::{LineView, emit_equal_runs};
use super::scan::{LineOff, count_text_lines, fill_line_offsets};
use super::scratch::{ConquerJob, align_scratch, layout_sizes, require_scratch};
use super::{EqualRun, LcsVisitResult, LineDiffError};

/// Visit equal index ranges in deterministic Myers order.
///
/// Line scanning writes offsets into `scratch`. The algorithm never builds
/// `Vec<String>` inputs or a `Vec<(usize, usize)>` match set. Work is the
/// number of Myers line comparisons actually performed. A visitor `Err`
/// stops promptly.
pub fn visit_lcs_equal_runs<E>(
    old_bytes: &[u8],
    new_bytes: &[u8],
    scratch: &mut [u8],
    budget: &mut ResourceBudget,
    visit: impl FnMut(EqualRun) -> Result<(), E>,
) -> LcsVisitResult<E> {
    let old_lines = count_text_lines(old_bytes).map_err(|_| LineDiffError::InvalidUtf8)?;
    let new_lines = count_text_lines(new_bytes).map_err(|_| LineDiffError::InvalidUtf8)?;
    budget.require(ResourceKind::Lines, old_lines as u64)?;
    budget.require(ResourceKind::Lines, new_lines as u64)?;

    let (aligned, pad) = align_scratch(scratch)?;
    let (needed, layout) = layout_sizes(old_lines, new_lines);
    budget.require(ResourceKind::ScratchBytes, (pad + needed) as u64)?;
    require_scratch(aligned.len(), needed)?;
    let scratch = &mut aligned[..needed];
    scratch.fill(0);

    let parts = unsafe { partition(scratch, &layout)? };
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
        budget,
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
/// Safety: `scratch` starts at [`super::scratch::max_scratch_align`] (proven
/// from the actual pointer by [`align_scratch`]). `layout` offsets were
/// produced by [`layout_sizes`] relative to that aligned base, do not overlap,
/// and each region start is checked against `align_of::<T>()` before the
/// cast. A misaligned region is a typed [`super::LineDiffError::BudgetExceeded`],
/// not UB.
unsafe fn partition<'a>(
    scratch: &'a mut [u8],
    layout: &super::scratch::ScratchLayout,
) -> Result<ScratchParts<'a>, super::super::budget::BudgetExceeded> {
    let base = scratch.as_mut_ptr();
    unsafe {
        Ok(ScratchParts {
            old_offs: raw_slice(base, layout.old_off, layout.old_off_bytes)?,
            new_offs: raw_slice(base, layout.new_off, layout.new_off_bytes)?,
            vf: raw_slice(base, layout.vf, layout.vf_bytes)?,
            vb: raw_slice(base, layout.vb, layout.vb_bytes)?,
            jobs: raw_slice(base, layout.jobs, layout.jobs_bytes)?,
        })
    }
}

unsafe fn raw_slice<'a, T>(
    base: *mut u8,
    start: usize,
    bytes: usize,
) -> Result<&'a mut [T], super::super::budget::BudgetExceeded> {
    let ptr = unsafe { base.add(start) };
    let addr = ptr as usize;
    if !addr.is_multiple_of(align_of::<T>()) {
        return Err(super::super::budget::BudgetExceeded {
            kind: ResourceKind::ScratchBytes,
            limit: addr as u64,
            needed: align_of::<T>() as u64,
        });
    }
    let count = if std::mem::size_of::<T>() == 0 {
        0
    } else {
        bytes / std::mem::size_of::<T>()
    };
    Ok(unsafe { std::slice::from_raw_parts_mut(ptr.cast::<T>(), count) })
}
