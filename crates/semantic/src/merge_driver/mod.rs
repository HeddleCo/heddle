// SPDX-License-Identifier: Apache-2.0
//! Function-level three-way merge driver.
//!
//! Decomposes a parseable source file into AST-defined items, merges each item
//! independently, and falls back to `heddle-merge::text_hunk_merge` on items
//! that can't be resolved structurally — and on the entire file when the
//! parser declines.
//!
//! See `docs/design/semantic-merge-function-level.md` for the contract.

use std::path::Path;

use merge::{ConflictMarkers, MergeOutcome, text_hunk_merge_with_markers};

use crate::parser::{Language, ParsedFile};

mod items;
mod reconstruct;

#[cfg(test)]
mod tests;

use items::segment_file;
use reconstruct::reconstruct_merged_file;

/// Three-way merge of `base`, `ours`, `theirs` using AST-defined item boundaries
/// when the parser accepts all three sides, falling back to
/// [`text_hunk_merge_with_markers`] otherwise.
///
/// The `path` is used for language detection only; it does NOT need to exist
/// on disk.
pub fn semantic_three_way_merge(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    path: &Path,
    markers: ConflictMarkers<'_>,
) -> MergeOutcome {
    if base == ours && base == theirs {
        return MergeOutcome::Clean(base.to_vec());
    }
    if base == ours {
        return MergeOutcome::Clean(theirs.to_vec());
    }
    if base == theirs {
        return MergeOutcome::Clean(ours.to_vec());
    }
    if ours == theirs {
        return MergeOutcome::Clean(ours.to_vec());
    }

    let language = Language::from_path(path);
    if matches!(language, Language::Unknown) {
        return text_hunk_merge_with_markers(base, ours, theirs, markers);
    }

    let (Ok(base_text), Ok(ours_text), Ok(theirs_text)) = (
        std::str::from_utf8(base),
        std::str::from_utf8(ours),
        std::str::from_utf8(theirs),
    ) else {
        return text_hunk_merge_with_markers(base, ours, theirs, markers);
    };

    let (Some(base_parsed), Some(ours_parsed), Some(theirs_parsed)) = (
        ParsedFile::parse(base_text, language),
        ParsedFile::parse(ours_text, language),
        ParsedFile::parse(theirs_text, language),
    ) else {
        return text_hunk_merge_with_markers(base, ours, theirs, markers);
    };

    let base_segments = segment_file(&base_parsed);
    let ours_segments = segment_file(&ours_parsed);
    let theirs_segments = segment_file(&theirs_parsed);

    reconstruct_merged_file(
        base_text,
        ours_text,
        theirs_text,
        &base_segments,
        &ours_segments,
        &theirs_segments,
        markers,
    )
}

/// Strategy a merge call should use for content reconciliation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStrategy {
    /// Always use `heddle-merge::text_hunk_merge` on the whole file.
    HunkOnly,
    /// Try AST-defined item decomposition first; fall through to
    /// `text_hunk_merge` for unparseable / unknown-language files.
    Semantic,
}

/// Single entry point that dispatches on [`MergeStrategy`]. Provided so call
/// sites that already thread a strategy enum don't have to branch themselves.
pub fn three_way_merge(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    path: &Path,
    markers: ConflictMarkers<'_>,
    strategy: MergeStrategy,
) -> MergeOutcome {
    match strategy {
        MergeStrategy::HunkOnly => text_hunk_merge_with_markers(base, ours, theirs, markers),
        MergeStrategy::Semantic => semantic_three_way_merge(base, ours, theirs, path, markers),
    }
}

pub use merge::{ConflictMarkers as MergeConflictMarkers, MergeOutcome as MergeDriverOutcome};
