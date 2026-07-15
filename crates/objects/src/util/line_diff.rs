// SPDX-License-Identifier: Apache-2.0
//! Shared line-oriented diff primitives used by native and Git-backed blame.

use similar::{Algorithm, DiffOp};

/// Split UTF-8 content into the same logical lines used by blame.
pub fn split_text_lines(bytes: &[u8]) -> Option<Vec<String>> {
    let content = std::str::from_utf8(bytes).ok()?;
    Some(content.lines().map(str::to_string).collect())
}

/// Return matching `(old, new)` line indexes from a stable, linear-space diff.
///
/// This is the single alignment primitive used by native provenance and
/// Git-overlay blame. Myers keeps memory proportional to the input lengths;
/// the former full LCS matrix used memory proportional to their product.
pub fn lcs_line_matches(old_lines: &[String], new_lines: &[String]) -> Vec<(usize, usize)> {
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

#[cfg(test)]
mod tests {
    use super::lcs_line_matches;

    #[test]
    fn line_matches_preserve_simple_alignment() {
        let old = ["a", "b", "c"].map(str::to_string);
        let new = ["a", "x", "c"].map(str::to_string);
        assert_eq!(lcs_line_matches(&old, &new), vec![(0, 0), (2, 2)]);
    }

    #[test]
    fn line_matches_are_bounded_for_large_files() {
        // The former (n + 1) * (m + 1) u32 matrix would require about
        // 10 GiB for this fixture. The shared Myers implementation stays
        // linear in the 50k-line inputs and preserves every unchanged line.
        let old = (0..50_000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>();
        let mut new = old.clone();
        new[25_000] = "replacement".to_string();

        let matches = lcs_line_matches(&old, &new);
        assert_eq!(matches.len(), 49_999);
        assert_eq!(matches.first(), Some(&(0, 0)));
        assert_eq!(matches.last(), Some(&(49_999, 49_999)));
        assert!(!matches.contains(&(25_000, 25_000)));
    }
}
