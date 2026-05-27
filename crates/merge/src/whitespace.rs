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

/// Check if two slices are trailing-ws-equal and if so return the cleaner
/// one (less total trailing whitespace). Single pass over both slices.
pub(super) fn compare_trailing_ws<'a>(
    a: &'a [&'a [u8]],
    b: &'a [&'a [u8]],
) -> Option<&'a [&'a [u8]]> {
    if a.len() != b.len() {
        return None;
    }
    let mut a_ws = 0usize;
    let mut b_ws = 0usize;
    for (x, y) in a.iter().zip(b.iter()) {
        if trailing_ws_key(x) != trailing_ws_key(y) {
            return None;
        }
        a_ws += count_trailing_ws(x);
        b_ws += count_trailing_ws(y);
    }
    Some(if a_ws <= b_ws { a } else { b })
}

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

fn count_trailing_ws(line: &[u8]) -> usize {
    let (body, _) = split_line_ending(line);
    body.iter()
        .rev()
        .take_while(|&&b| b == b' ' || b == b'\t')
        .count()
}
