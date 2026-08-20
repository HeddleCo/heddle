// SPDX-License-Identifier: Apache-2.0
//! Exact linear-space Myers. Heuristic approximation is not used; if the
//! search cannot finish inside the work budget it returns BudgetExceeded.

use super::super::budget::{BudgetExceeded, ResourceBudget, ResourceKind};
use super::emit::{EqualEmit, emit_slid, flush_pending};
use super::myers_search::{common_prefix, common_suffix, find_middle_snake};
use super::scan::LineOff;
use super::scratch::{ConquerJob, JOB_EQUAL, JOB_RANGE};
use super::{EqualRun, LineDiffError};

#[derive(Clone, Copy)]
pub(super) struct LineView<'a> {
    pub bytes: &'a [u8],
    pub offs: &'a [LineOff],
}

pub(super) fn emit_equal_runs<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    vf_storage: &mut [usize],
    vb_storage: &mut [usize],
    jobs: &mut [ConquerJob],
    budget: &mut ResourceBudget,
    mut visit: impl FnMut(EqualRun) -> Result<(), E>,
) -> Result<(), LineDiffError<E>> {
    if jobs.is_empty() {
        return Ok(());
    }
    let mut top = 0usize;
    push_job(
        jobs,
        &mut top,
        ConquerJob {
            old_lo: 0,
            old_hi: old.offs.len() as u32,
            new_lo: 0,
            new_hi: new.offs.len() as u32,
            kind: JOB_RANGE,
            eq_old: 0,
            eq_new: 0,
            eq_len: 0,
        },
    )?;

    let mut new_cursor = 0usize;
    let mut pending = None;
    let mut emit = EqualEmit {
        new_cursor: &mut new_cursor,
        pending: &mut pending,
    };
    while top > 0 {
        top -= 1;
        let job = jobs[top];
        if job.kind == JOB_EQUAL {
            if job.eq_len == 0 {
                continue;
            }
            emit_slid(
                old,
                new,
                EqualRun {
                    old_start: job.eq_old as usize,
                    new_start: job.eq_new as usize,
                    len: job.eq_len as usize,
                },
                &mut emit,
                &mut visit,
            )?;
            continue;
        }
        conquer_range(
            old,
            new,
            ScratchViews {
                vf: vf_storage,
                vb: vb_storage,
                jobs,
                top: &mut top,
            },
            job,
            budget,
            &mut emit,
            &mut visit,
        )?;
    }
    flush_pending(old, new, emit.pending.take(), new.offs.len(), &mut visit)
}

struct ScratchViews<'a> {
    vf: &'a mut [usize],
    vb: &'a mut [usize],
    jobs: &'a mut [ConquerJob],
    top: &'a mut usize,
}

struct EqualEmit<'a> {
    new_cursor: &'a mut usize,
    pending: &'a mut Option<PendingEqual>,
}

fn conquer_range<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    scratch: ScratchViews<'_>,
    job: ConquerJob,
    budget: &mut ResourceBudget,
    emit: &mut EqualEmit<'_>,
    visit: &mut impl FnMut(EqualRun) -> Result<(), E>,
) -> Result<(), LineDiffError<E>> {
    let ScratchViews {
        vf: vf_storage,
        vb: vb_storage,
        jobs,
        top,
    } = scratch;
    let mut old_lo = job.old_lo as usize;
    let mut old_hi = job.old_hi as usize;
    let mut new_lo = job.new_lo as usize;
    let mut new_hi = job.new_hi as usize;

    let prefix = common_prefix(old, old_lo, old_hi, new, new_lo, new_hi, budget)?;
    if prefix > 0 {
        emit_slid(
            old,
            new,
            EqualRun {
                old_start: old_lo,
                new_start: new_lo,
                len: prefix,
            },
            emit,
            visit,
        )?;
        old_lo += prefix;
        new_lo += prefix;
    }

    // Same order as similar: suffix after prefix, then middle-snake.
    let suffix = common_suffix(old, old_lo, old_hi, new, new_lo, new_hi, budget)?;
    let suffix_old = old_hi - suffix;
    let suffix_new = new_hi - suffix;
    old_hi -= suffix;
    new_hi -= suffix;

    if old_lo >= old_hi || new_lo >= new_hi {
        if suffix > 0 {
            push_job(jobs, top, equal_job(suffix_old, suffix_new, suffix))?;
        }
        return Ok(());
    }

    let Some((split_old, split_new)) = find_middle_snake(
        old,
        old_lo..old_hi,
        new,
        new_lo..new_hi,
        vf_storage,
        vb_storage,
        budget,
    )?
    else {
        return Err(LineDiffError::from_budget(BudgetExceeded {
            kind: ResourceKind::Work,
            limit: budget.limit(ResourceKind::Work),
            needed: budget.used().work.saturating_add(1),
        }));
    };

    if suffix > 0 {
        push_job(jobs, top, equal_job(suffix_old, suffix_new, suffix))?;
    }
    push_job(
        jobs,
        top,
        ConquerJob {
            old_lo: split_old as u32,
            old_hi: old_hi as u32,
            new_lo: split_new as u32,
            new_hi: new_hi as u32,
            kind: JOB_RANGE,
            eq_old: 0,
            eq_new: 0,
            eq_len: 0,
        },
    )?;
    push_job(
        jobs,
        top,
        ConquerJob {
            old_lo: old_lo as u32,
            old_hi: split_old as u32,
            new_lo: new_lo as u32,
            new_hi: split_new as u32,
            kind: JOB_RANGE,
            eq_old: 0,
            eq_new: 0,
            eq_len: 0,
        },
    )?;
    Ok(())
}

fn equal_job(old_start: usize, new_start: usize, len: usize) -> ConquerJob {
    ConquerJob {
        old_lo: 0,
        old_hi: 0,
        new_lo: 0,
        new_hi: 0,
        kind: JOB_EQUAL,
        eq_old: old_start as u32,
        eq_new: new_start as u32,
        eq_len: len as u32,
    }
}

fn push_job<E>(
    jobs: &mut [ConquerJob],
    top: &mut usize,
    job: ConquerJob,
) -> Result<(), LineDiffError<E>> {
    if *top >= jobs.len() {
        return Err(LineDiffError::from_budget(BudgetExceeded {
            kind: ResourceKind::ScratchBytes,
            limit: jobs.len() as u64,
            needed: (*top as u64).saturating_add(1),
        }));
    }
    jobs[*top] = job;
    *top += 1;
    Ok(())
}
