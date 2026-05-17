// SPDX-License-Identifier: Apache-2.0
//! Whitespace-equivalence helpers used during conflict classification.
//!
//! Policy: when both sides modify a hunk and the only difference is *trailing*
//! whitespace (spaces / tabs) on otherwise-equal lines, the two sides are
//! treated as having made the same change and the merge picks the side with
//! less trailing whitespace. This matches the brief's guidance and avoids
//! spurious conflicts from autoformatters.
//!
//! Line endings (`\r\n` vs `\n`) are **load-bearing** and NOT folded into the
//! whitespace-equivalence — a CRLF-only side and an LF-only side editing the
//! same hunk produce a real conflict so platform divergence surfaces to the
//! user. Only intra-line trailing whitespace (spaces and tabs *before* the
//! line ending) is stripped for the comparison.

pub(super) fn trailing_ws_equal(a: &[&[u8]], b: &[&[u8]]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| trailing_ws_key(x) == trailing_ws_key(y))
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

/// Comparison key that strips intra-line trailing whitespace but preserves the
/// line-ending bytes — so two lines compare equal iff they agree on body (mod
/// trailing space/tab) AND on terminator (LF / CRLF / none).
fn trailing_ws_key(line: &[u8]) -> (&[u8], &[u8]) {
    let (body, ending) = split_line_ending(line);
    let stripped = body.iter().rposition(|&b| b != b' ' && b != b'\t');
    let stripped_body = match stripped {
        Some(idx) => &body[..=idx],
        None => &body[..0],
    };
    (stripped_body, ending)
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
