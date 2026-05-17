// SPDX-License-Identifier: Apache-2.0
//! diff3 algorithm: line-level three-way merge with per-hunk conflict markers.
//!
//! NOTE: stub. Real implementation lands in the next commit; this exists so the
//! red-commit fixtures in `merge::tests` compile and fail loudly.

use super::MergeOutcome;

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

pub(super) fn run(
    _base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    let mut out = Vec::new();
    out.extend_from_slice(b"<<<<<<< ");
    out.extend_from_slice(markers.ours.as_bytes());
    out.push(b'\n');
    out.extend_from_slice(ours);
    if !ours.ends_with(b"\n") && !ours.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(b"=======\n");
    out.extend_from_slice(theirs);
    if !theirs.ends_with(b"\n") && !theirs.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(b">>>>>>> ");
    out.extend_from_slice(markers.theirs.as_bytes());
    out.push(b'\n');
    MergeOutcome::Conflicts {
        merged_bytes_with_markers: out,
        conflict_count: 1,
    }
}
