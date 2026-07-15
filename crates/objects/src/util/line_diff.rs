// SPDX-License-Identifier: Apache-2.0
//! Shared line-oriented diff primitives used by native and Git-backed blame.

/// Split UTF-8 content into the same logical lines used by blame.
pub fn split_text_lines(bytes: &[u8]) -> Option<Vec<String>> {
    let content = std::str::from_utf8(bytes).ok()?;
    Some(content.lines().map(str::to_string).collect())
}

/// Return matching `(old, new)` line indexes from a stable LCS walk.
pub fn lcs_line_matches(old_lines: &[String], new_lines: &[String]) -> Vec<(usize, usize)> {
    let n = old_lines.len();
    let m = new_lines.len();
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if old_lines[i] == new_lines[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut i = 0usize;
    let mut j = 0usize;
    let mut matches = Vec::new();
    while i < n && j < m {
        if old_lines[i] == new_lines[j] {
            matches.push((i, j));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    matches
}
