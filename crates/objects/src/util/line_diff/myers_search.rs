// SPDX-License-Identifier: Apache-2.0
//! Middle-snake search and line equality for exact Myers.

use super::super::budget::{BudgetExceeded, ResourceBudget, ResourceKind};
use super::myers::LineView;
use super::scan::line_bytes;

struct V<'a> {
    offset: isize,
    values: &'a mut [usize],
}

impl V<'_> {
    fn get(&self, k: isize) -> usize {
        self.values[(k + self.offset) as usize]
    }

    fn set(&mut self, k: isize, value: usize) {
        self.values[(k + self.offset) as usize] = value;
    }
}

fn lines_eq(
    old: LineView<'_>,
    old_index: usize,
    new: LineView<'_>,
    new_index: usize,
    budget: &mut ResourceBudget,
) -> Result<bool, BudgetExceeded> {
    budget.consume(ResourceKind::Work, 1)?;
    Ok(line_bytes(old.bytes, old.offs[old_index]) == line_bytes(new.bytes, new.offs[new_index]))
}

pub(super) fn common_prefix(
    old: LineView<'_>,
    old_lo: usize,
    old_hi: usize,
    new: LineView<'_>,
    new_lo: usize,
    new_hi: usize,
    budget: &mut ResourceBudget,
) -> Result<usize, BudgetExceeded> {
    let max_len = (old_hi - old_lo).min(new_hi - new_lo);
    let mut matched = 0;
    while matched < max_len && lines_eq(old, old_lo + matched, new, new_lo + matched, budget)? {
        matched += 1;
    }
    Ok(matched)
}

pub(super) fn common_suffix(
    old: LineView<'_>,
    old_lo: usize,
    old_hi: usize,
    new: LineView<'_>,
    new_lo: usize,
    new_hi: usize,
    budget: &mut ResourceBudget,
) -> Result<usize, BudgetExceeded> {
    let max_len = (old_hi - old_lo).min(new_hi - new_lo);
    let mut matched = 0;
    while matched < max_len
        && lines_eq(old, old_hi - 1 - matched, new, new_hi - 1 - matched, budget)?
    {
        matched += 1;
    }
    Ok(matched)
}

pub(super) fn find_middle_snake(
    old: LineView<'_>,
    old_range: std::ops::Range<usize>,
    new: LineView<'_>,
    new_range: std::ops::Range<usize>,
    vf_storage: &mut [usize],
    vb_storage: &mut [usize],
    budget: &mut ResourceBudget,
) -> Result<Option<(usize, usize)>, BudgetExceeded> {
    let n = old_range.len();
    let m = new_range.len();
    if n == 0 || m == 0 {
        return Ok(Some((old_range.start, new_range.start)));
    }

    let delta = n as isize - m as isize;
    let odd = delta & 1 == 1;
    let d_max = n.saturating_add(m).div_ceil(2).saturating_add(1);
    let offset = (vf_storage.len() / 2) as isize;
    let mut vf = V {
        offset,
        values: vf_storage,
    };
    let mut vb = V {
        offset,
        values: vb_storage,
    };
    vf.set(1, 0);
    vb.set(1, 0);

    for d in 0..d_max as isize {
        for k in (-d..=d).rev().step_by(2) {
            let mut x = if k == -d || (k != d && vf.get(k - 1) < vf.get(k + 1)) {
                vf.get(k + 1)
            } else {
                vf.get(k - 1) + 1
            };
            let y = (x as isize - k) as usize;
            let (x0, y0) = (x, y);
            if x < n && y < m {
                x += common_prefix(
                    old,
                    old_range.start + x,
                    old_range.end,
                    new,
                    new_range.start + y,
                    new_range.end,
                    budget,
                )?;
            }
            vf.set(k, x);
            if odd && (k - delta).abs() <= (d - 1) && vf.get(k) + vb.get(-(k - delta)) >= n {
                return Ok(Some((x0 + old_range.start, y0 + new_range.start)));
            }
        }

        for k in (-d..=d).rev().step_by(2) {
            let mut x = if k == -d || (k != d && vb.get(k - 1) < vb.get(k + 1)) {
                vb.get(k + 1)
            } else {
                vb.get(k - 1) + 1
            };
            let mut y = (x as isize - k) as usize;
            if x < n && y < m {
                let advance = common_suffix(
                    old,
                    old_range.start,
                    old_range.start + n - x,
                    new,
                    new_range.start,
                    new_range.start + m - y,
                    budget,
                )?;
                x += advance;
                y += advance;
            }
            vb.set(k, x);
            if !odd && (k - delta).abs() <= d && vb.get(k) + vf.get(-(k - delta)) >= n {
                return Ok(Some((n - x + old_range.start, m - y + new_range.start)));
            }
        }
    }
    Ok(None)
}
