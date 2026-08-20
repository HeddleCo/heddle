// SPDX-License-Identifier: Apache-2.0
//! Compatibility collector used by tree-oriented provenance builders.

use super::scratch::scratch_bytes_for_line_counts;
use super::visit::visit_lcs_equal_runs;
use super::{LineDiffError, LineDiffLimits};

/// Return matching `(old, new)` line indexes from the canonical equal-run LCS.
///
/// This is the same alignment [`super::visit_lcs_equal_runs`] emits, expanded
/// to per-line pairs for existing snapshot/merge callers.
pub fn lcs_line_matches(
    old_lines: &[String],
    new_lines: &[String],
) -> Result<Vec<(usize, usize)>, LineDiffError> {
    let old = old_lines.join("\n");
    let new = new_lines.join("\n");
    collect_byte_matches(
        old.as_bytes(),
        new.as_bytes(),
        old_lines.len(),
        new_lines.len(),
    )
}

fn collect_byte_matches(
    old_bytes: &[u8],
    new_bytes: &[u8],
    old_lines: usize,
    new_lines: usize,
) -> Result<Vec<(usize, usize)>, LineDiffError> {
    let needed = scratch_bytes_for_line_counts(old_lines, new_lines);
    let mut scratch = vec![0u8; needed];
    let mut matches = Vec::new();
    let result = visit_lcs_equal_runs(
        old_bytes,
        new_bytes,
        &mut scratch,
        LineDiffLimits::unlimited(),
        |run| {
            for offset in 0..run.len {
                matches.push((run.old_start + offset, run.new_start + offset));
            }
            Ok::<(), std::convert::Infallible>(())
        },
    );
    match result {
        Ok(_) => Ok(matches),
        Err(LineDiffError::BudgetExceeded(error)) => {
            let mut scratch = vec![0u8; error.needed as usize];
            matches.clear();
            visit_lcs_equal_runs(
                old_bytes,
                new_bytes,
                &mut scratch,
                LineDiffLimits::unlimited(),
                |run| {
                    for offset in 0..run.len {
                        matches.push((run.old_start + offset, run.new_start + offset));
                    }
                    Ok::<(), std::convert::Infallible>(())
                },
            )?;
            Ok(matches)
        }
        Err(error) => Err(error),
    }
}
