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
mod language_rules;
mod reconstruct;

#[cfg(test)]
mod tests;

use items::{InstanceAlignment, align_container_instances, segment_file};
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

    let mut base_segments = segment_file(&base_parsed);
    let mut ours_segments = segment_file(&ours_parsed);
    let mut theirs_segments = segment_file(&theirs_parsed);

    // Rekey `use` items so declarations whose expanded leaf sets intersect
    // on ANY path collide for cross-side matching (heddle#468; Codex r2 on
    // PR #477). Must run before the empty-base add/add guard below and
    // before reconstruction, both of which key off `Item`/`ItemKey`.
    items::canonicalize_use_keys(&mut base_segments, &mut ours_segments, &mut theirs_segments);

    // When a side has zero parseable items but the others do, the
    // per-item alignment has nothing to anchor on for that side and
    // its contiguous content can't be cleanly split across the other
    // sides' per-item segments — the surrounding preamble/postamble
    // merges either drop the side's edits (Codex r2 P1 #3) or
    // double-emit its bridging content. text_hunk_merge handles the
    // full-file alignment without those artifacts, so route this
    // shape through it.
    //
    // EXCEPTION: empty base with both sides adding items that share
    // keys (add/add). text_hunk_merge concatenates both insertions
    // at the same anchor and silently produces duplicate definitions;
    // `resolve_item`'s add/add arm is the only path that surfaces this
    // as a conflict. Drop through to the reconstruct path in that case
    // so the conflict is reported (Codex r3 P1 #1).
    let counts = [
        base_segments.items.len(),
        ours_segments.items.len(),
        theirs_segments.items.len(),
    ];
    if counts.contains(&0) && counts.iter().any(|&c| c > 0) {
        let addadd_in_empty_base = base_segments.items.is_empty() && {
            let ours_keys: std::collections::BTreeSet<_> =
                ours_segments.items.iter().map(|i| &i.key).collect();
            theirs_segments
                .items
                .iter()
                .any(|i| ours_keys.contains(&i.key))
        };
        if !addadd_in_empty_base {
            return text_hunk_merge_with_markers(base, ours, theirs, markers);
        }
    }

    // Anchor each side's container-instance ordinals to base spans so a
    // prepended / appended / reordered same-name container keeps an identity
    // distinct from the matched base block (heddle#484 r3, part 1). When the
    // matched-item correspondence is non-bijective — a side merged two base
    // containers into one, split one across two, or moved a matched item
    // between containers — the instance model cannot decide whether two
    // same-name spans are one instance or two. Rather than risk a silent
    // collapse, route to the textual conflict path: a conflict the user
    // resolves is safe; a silent collapse is the P0 (part 2).
    if let InstanceAlignment::Ambiguous =
        align_container_instances(&base_segments, &mut ours_segments, &mut theirs_segments)
    {
        return text_hunk_merge_with_markers(base, ours, theirs, markers);
    }

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
