// SPDX-License-Identifier: Apache-2.0
//! Exact linear-space Myers. Heuristic approximation is not used; if the
//! search cannot finish inside the work budget it returns BudgetExceeded.

use super::super::budget::{BudgetExceeded, ResourceBudget, ResourceKind};
use super::myers_search::{common_prefix, find_middle_snake};
use super::scan::LineOff;
use super::scratch::ConquerJob;
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
        },
    )?;

    while top > 0 {
        top -= 1;
        let job = jobs[top];
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
            &mut visit,
        )?;
    }
    Ok(())
}

struct ScratchViews<'a> {
    vf: &'a mut [usize],
    vb: &'a mut [usize],
    jobs: &'a mut [ConquerJob],
    top: &'a mut usize,
}

fn conquer_range<E>(
    old: LineView<'_>,
    new: LineView<'_>,
    scratch: ScratchViews<'_>,
    job: ConquerJob,
    budget: &mut ResourceBudget,
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
        visit(EqualRun {
            old_start: old_lo,
            new_start: new_lo,
            len: prefix,
        })
        .map_err(LineDiffError::Visitor)?;
        old_lo += prefix;
        new_lo += prefix;
    }

    if old_lo >= old_hi || new_lo >= new_hi {
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

    push_job(
        jobs,
        top,
        ConquerJob {
            old_lo: split_old as u32,
            old_hi: old_hi as u32,
            new_lo: split_new as u32,
            new_hi: new_hi as u32,
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
        },
    )?;
    Ok(())
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
