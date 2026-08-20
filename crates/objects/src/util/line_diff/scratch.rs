// SPDX-License-Identifier: Apache-2.0
//! Caller-scratch layout for line offsets, Myers V arrays, and the conquer stack.

use std::mem::{align_of, size_of};

use super::super::budget::{BudgetExceeded, ResourceKind};
use super::scan::LineOff;

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct ConquerJob {
    pub old_lo: u32,
    pub old_hi: u32,
    pub new_lo: u32,
    pub new_hi: u32,
    pub kind: u8,
    pub eq_old: u32,
    pub eq_new: u32,
    pub eq_len: u32,
}

pub(super) const JOB_RANGE: u8 = 0;
pub(super) const JOB_EQUAL: u8 = 1;

pub fn max_scratch_align() -> usize {
    align_of::<LineOff>()
        .max(align_of::<usize>())
        .max(align_of::<ConquerJob>())
}

pub fn scratch_bytes_for_line_counts(old_lines: usize, new_lines: usize) -> usize {
    let (needed, _) = layout_sizes(old_lines, new_lines);
    needed.saturating_add(max_scratch_align().saturating_sub(1))
}

#[cfg(test)]
pub(super) fn aligned_layout_bytes(old_lines: usize, new_lines: usize) -> usize {
    let (needed, _) = layout_sizes(old_lines, new_lines);
    needed
}

/// Shift `scratch` so the returned suffix starts at [`max_scratch_align`].
///
/// The pad is computed from the actual pointer, not from a layout that
/// assumed the slice was already aligned.
pub(super) fn align_scratch(scratch: &mut [u8]) -> Result<(&mut [u8], usize), BudgetExceeded> {
    let align = max_scratch_align();
    let addr = scratch.as_mut_ptr() as usize;
    let pad = if align <= 1 {
        0
    } else {
        (align - (addr % align)) % align
    };
    if pad > scratch.len() {
        return Err(BudgetExceeded {
            kind: ResourceKind::ScratchBytes,
            limit: scratch.len() as u64,
            needed: (pad as u64).saturating_add(1),
        });
    }
    Ok((&mut scratch[pad..], pad))
}

pub(super) struct ScratchLayout {
    pub old_off: usize,
    pub new_off: usize,
    pub vf: usize,
    pub vb: usize,
    pub jobs: usize,
    pub old_off_bytes: usize,
    pub new_off_bytes: usize,
    pub vf_bytes: usize,
    pub vb_bytes: usize,
    pub jobs_bytes: usize,
}

pub(super) fn layout_sizes(old_lines: usize, new_lines: usize) -> (usize, ScratchLayout) {
    let max_d = max_d(old_lines, new_lines);
    let v_len = 2 * max_d;
    let job_cap = old_lines.saturating_add(new_lines).saturating_add(8);

    let mut cursor = 0usize;
    let old_off = align_up(cursor, align_of::<LineOff>());
    let old_off_bytes = old_lines.saturating_mul(size_of::<LineOff>());
    cursor = old_off.saturating_add(old_off_bytes);

    let new_off = align_up(cursor, align_of::<LineOff>());
    let new_off_bytes = new_lines.saturating_mul(size_of::<LineOff>());
    cursor = new_off.saturating_add(new_off_bytes);

    let vf = align_up(cursor, align_of::<usize>());
    let vf_bytes = v_len.saturating_mul(size_of::<usize>());
    cursor = vf.saturating_add(vf_bytes);

    let vb = align_up(cursor, align_of::<usize>());
    let vb_bytes = v_len.saturating_mul(size_of::<usize>());
    cursor = vb.saturating_add(vb_bytes);

    let jobs = align_up(cursor, align_of::<ConquerJob>());
    let jobs_bytes = job_cap.saturating_mul(size_of::<ConquerJob>());
    cursor = jobs.saturating_add(jobs_bytes);

    (
        cursor,
        ScratchLayout {
            old_off,
            new_off,
            vf,
            vb,
            jobs,
            old_off_bytes,
            new_off_bytes,
            vf_bytes,
            vb_bytes,
            jobs_bytes,
        },
    )
}

pub(super) fn max_d(old_lines: usize, new_lines: usize) -> usize {
    old_lines
        .saturating_add(new_lines)
        .div_ceil(2)
        .saturating_add(1)
}

fn align_up(value: usize, align: usize) -> usize {
    if align <= 1 {
        return value;
    }
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

pub(super) fn require_scratch(scratch_len: usize, needed: usize) -> Result<(), BudgetExceeded> {
    if needed > scratch_len {
        return Err(BudgetExceeded {
            kind: ResourceKind::ScratchBytes,
            limit: scratch_len as u64,
            needed: needed as u64,
        });
    }
    Ok(())
}
