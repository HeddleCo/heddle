// SPDX-License-Identifier: Apache-2.0
//! Whitespace-equivalence helpers used during conflict classification.
//!
//! Policy: when both sides modify a hunk and the only difference is trailing
//! whitespace (spaces / tabs) on otherwise-equal lines, the two sides are
//! treated as having made the same change and the merge picks the side with
//! less trailing whitespace. This matches the brief's guidance and avoids
//! spurious conflicts from autoformatters.

/// True if `a` and `b` would be equal after stripping trailing whitespace
/// (` `, `\t`) from each line (line endings preserved or absent equally).
pub(super) fn trailing_ws_equal(a: &[&[u8]], b: &[&[u8]]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| strip_trailing_ws(x) == strip_trailing_ws(y))
}

/// Of two whitespace-equivalent line slices, return the one with less total
/// trailing whitespace across all lines (preferring cleaner output).
pub(super) fn prefer_clean<'a>(a: &'a [&'a [u8]], b: &'a [&'a [u8]]) -> &'a [&'a [u8]] {
    if total_trailing_ws(a) <= total_trailing_ws(b) {
        a
    } else {
        b
    }
}

fn strip_trailing_ws(line: &[u8]) -> &[u8] {
    let (body, _ending) = split_line_ending(line);
    let stripped = body.iter().rposition(|&b| b != b' ' && b != b'\t');
    match stripped {
        Some(idx) => &body[..=idx],
        None => &body[..0],
    }
}

fn split_line_ending(line: &[u8]) -> (&[u8], &[u8]) {
    if let Some(stripped) = line.strip_suffix(b"\r\n") {
        (stripped, b"\r\n")
    } else if let Some(stripped) = line.strip_suffix(b"\n") {
        (stripped, b"\n")
    } else {
        (line, b"")
    }
}

fn total_trailing_ws(slice: &[&[u8]]) -> usize {
    slice
        .iter()
        .map(|line| {
            let (body, _) = split_line_ending(line);
            body.iter()
                .rev()
                .take_while(|&&b| b == b' ' || b == b'\t')
                .count()
        })
        .sum()
}
