// SPDX-License-Identifier: Apache-2.0
//! Canonical conflict marker emission.
//!
//! Markers are always at column 0, with a `\n` immediately preceding `=======`
//! and `>>>>>>>`, matching git's convention and the heddle#78 validator.

/// Labels for the `<<<<<<<` / `>>>>>>>` markers emitted around conflict hunks.
///
/// `ours` labels the local side; `theirs` labels the incoming side.
#[derive(Clone, Copy, Debug)]
pub struct ConflictMarkers<'a> {
    pub ours: &'a str,
    pub theirs: &'a str,
}

impl ConflictMarkers<'_> {
    /// Default labels: `"CURRENT"` for ours, `"INCOMING"` for theirs.
    pub const DEFAULT: ConflictMarkers<'static> = ConflictMarkers {
        ours: "CURRENT",
        theirs: "INCOMING",
    };
}

impl Default for ConflictMarkers<'_> {
    fn default() -> Self {
        Self::DEFAULT
    }
}

pub(super) fn emit_lines(out: &mut Vec<u8>, lines: &[&[u8]]) {
    for line in lines {
        out.extend_from_slice(line);
    }
}

/// Append a `<<<<<<< / ======= / >>>>>>>` triple wrapping `ours` then `theirs`.
pub(super) fn emit_conflict(
    out: &mut Vec<u8>,
    our_slice: &[&[u8]],
    their_slice: &[&[u8]],
    markers: ConflictMarkers<'_>,
) {
    out.extend_from_slice(b"<<<<<<< ");
    out.extend_from_slice(markers.ours.as_bytes());
    out.push(b'\n');
    emit_lines(out, our_slice);
    ensure_trailing_newline(out);
    out.extend_from_slice(b"=======\n");
    emit_lines(out, their_slice);
    ensure_trailing_newline(out);
    out.extend_from_slice(b">>>>>>> ");
    out.extend_from_slice(markers.theirs.as_bytes());
    out.push(b'\n');
}

fn ensure_trailing_newline(out: &mut Vec<u8>) {
    if !out.is_empty() && *out.last().unwrap() != b'\n' {
        out.push(b'\n');
    }
}
