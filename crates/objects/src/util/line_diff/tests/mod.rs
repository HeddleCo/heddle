// SPDX-License-Identifier: Apache-2.0
mod alignment;
mod budget;

use similar::{Algorithm, DiffOp};

use super::{
    EqualRun, LineDiffError, LineDiffLimits, scratch_bytes_for_line_counts, visit_lcs_equal_runs,
};

fn collect_runs(
    old: &str,
    new: &str,
    limits: LineDiffLimits,
) -> Result<Vec<EqualRun>, LineDiffError> {
    let needed = scratch_bytes_for_line_counts(old.lines().count(), new.lines().count());
    let mut scratch = vec![0u8; needed.max(1)];
    let mut budget = limits.budget(scratch.len());
    let mut runs = Vec::new();
    visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        &mut budget,
        |run| {
            runs.push(run);
            Ok::<(), std::convert::Infallible>(())
        },
    )?;
    Ok(runs)
}

fn visit_usage(
    old: &str,
    new: &str,
    limits: LineDiffLimits,
) -> Result<crate::util::ResourceUsage, LineDiffError> {
    let needed = scratch_bytes_for_line_counts(old.lines().count(), new.lines().count());
    let mut scratch = vec![0u8; needed.max(1)];
    let mut budget = limits.budget(scratch.len());
    visit_lcs_equal_runs(
        old.as_bytes(),
        new.as_bytes(),
        &mut scratch,
        &mut budget,
        |_| Ok::<(), std::convert::Infallible>(()),
    )
}

fn expand_runs(runs: &[EqualRun]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    for run in runs {
        for offset in 0..run.len {
            matches.push((run.old_start + offset, run.new_start + offset));
        }
    }
    matches
}

fn similar_pairs(old_lines: &[String], new_lines: &[String]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    for op in similar::capture_diff_slices(Algorithm::Myers, old_lines, new_lines) {
        if let DiffOp::Equal {
            old_index,
            new_index,
            len,
        } = op
        {
            matches.extend((0..len).map(|offset| (old_index + offset, new_index + offset)));
        }
    }
    matches
}
