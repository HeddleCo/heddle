// SPDX-License-Identifier: Apache-2.0
//! Native hunk-level three-way text merge.
//!
//! This crate provides [`text_hunk_merge`], heddle's own line-based three-way
//! merge engine. It is the layer the semantic merger and other tooling fall
//! through to when path- or symbol-level reconciliation declines. Unlike the
//! prior single-range merger that bailed to whole-file conflict markers on any
//! multi-hunk diff, this engine identifies disjoint hunks via diff3-style
//! alignment and emits per-hunk markers — matching git's baseline competence.
//!
//! The engine is a primitive: it depends only on `similar` for LCS alignment
//! and is intentionally split out of `heddle-semantic` so non-semantic CLI
//! builds (e.g. `--no-default-features`) retain text-level auto-merge. Higher
//! layers (semantic resolver, rebase replay, merge driver) call into this
//! crate unconditionally.
//!
//! ## Pipeline position
//!
//! 1. Semantic merge succeeds → resolved, no markers.
//! 2. Semantic merge declines → fall through to [`text_hunk_merge`].
//! 3. Hunk-level merge sees no conflicts (disjoint line ranges) → resolved.
//! 4. Hunk-level merge sees conflicts → per-hunk canonical markers.
//!
//! ## Marker format
//!
//! All conflict markers are emitted at column 0, with a `\n` immediately
//! preceding `=======` and `>>>>>>>` lines. This matches git's convention and
//! the validator described in heddle#78.
//!
//! ## Line-ending and whitespace policy
//!
//! - Line endings (CRLF / LF) are preserved verbatim — no normalization. A
//!   trailing-newline divergence between sides does not by itself produce a
//!   conflict; the side that introduced/retained content wins.
//! - When both sides modify the same hunk but the only difference is trailing
//!   whitespace on otherwise-equal lines, the merge prefers the version with
//!   no extra trailing whitespace to avoid spurious conflicts. CRLF vs LF is
//!   NOT treated as "whitespace-equivalent": line endings are load-bearing on
//!   cross-platform repos and divergence there must surface as a conflict.
//!
//! ## Binary files
//!
//! Inputs are classified as binary if any of the first 8 KiB contains a NUL
//! byte. Binary inputs return [`MergeOutcome::Binary`] — callers should fall
//! back to a whole-file conflict (matches git's `binary file changed in both`
//! shape).

mod diff3;
mod lines;
mod markers;
mod preflight;
mod whitespace;

#[cfg(test)]
mod tests;

pub use markers::ConflictMarkers;

/// Outcome of a three-way line-based merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Both sides merged cleanly with no conflicts. Inner bytes are the
    /// merged file content.
    Clean(Vec<u8>),
    /// Conflicts were found. Inner bytes contain the partially-merged file
    /// with per-hunk conflict markers inserted. `conflict_count` is the
    /// number of distinct marker triples in the output.
    Conflicts {
        merged_bytes_with_markers: Vec<u8>,
        conflict_count: usize,
    },
    /// One or more inputs is binary (NUL byte in first 8 KiB). Callers
    /// should emit a whole-file conflict.
    Binary,
    /// One side deleted the file while the other modified it. Reserved for
    /// callers that want to model this state through the same enum;
    /// [`text_hunk_merge`] never returns this variant because all three
    /// inputs are byte slices (deletion is detected at the tree level).
    DeleteVsModify,
}

/// Three-way line-based merge of `base`, `ours`, and `theirs`.
///
/// Returns [`MergeOutcome::Clean`] when the two sides' changes are
/// disjoint (or identical, or one-sided), [`MergeOutcome::Conflicts`]
/// when both sides modify overlapping line ranges differently, and
/// [`MergeOutcome::Binary`] if any input contains binary data.
///
/// Conflict markers use the default labels `"CURRENT"` / `"INCOMING"`.
/// Use [`text_hunk_merge_with_markers`] to override.
pub fn text_hunk_merge(base: &[u8], ours: &[u8], theirs: &[u8]) -> MergeOutcome {
    text_hunk_merge_with_markers(base, ours, theirs, ConflictMarkers::DEFAULT)
}

/// Three-way line-based merge with caller-supplied conflict marker labels.
///
/// See [`text_hunk_merge`] for the algorithm; this variant lets the caller
/// label the `<<<<<<<` / `>>>>>>>` markers (e.g. branch names).
pub fn text_hunk_merge_with_markers(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    if preflight::any_binary(base, ours, theirs) {
        return MergeOutcome::Binary;
    }
    diff3::run(base, ours, theirs, markers)
}
