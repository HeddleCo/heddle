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

use crate::{
    cache::SemanticParseCache,
    parser::{Language, ParsedFile},
};

mod items;
mod language_rules;
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

    // Share the process-wide parse cache with semantic diff so base/ours/theirs
    // are not thrice-cold on every merge when the same blobs were just parsed
    // for classification or a prior merge attempt.
    let cache = SemanticParseCache::shared();
    let (Some(base_parsed), Some(ours_parsed), Some(theirs_parsed)) = (
        cache.parse(base_text, language),
        cache.parse(ours_text, language),
        cache.parse(theirs_text, language),
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

    let outcome = reconstruct_merged_file(
        base_text,
        ours_text,
        theirs_text,
        &base_segments,
        &ours_segments,
        &theirs_segments,
        markers,
    );

    // Input-grounded safety net (heddle#490 P3 floor). The tree model makes
    // silent structural collapse impossible *by construction*, but a cheap
    // conservation check against the INPUTS — not the merge's own resolved
    // metadata — is kept as defense-in-depth: if a CLEAN merge ever fails to
    // re-parse or invents an item/nesting no input had, route to the textual
    // path instead of emitting the corruption.
    //
    // The floor also guards CONFLICT outputs (heddle#490 r6). The clean-only
    // check could not see a malformed body that ships *alongside* a real
    // conflict: a divergent container header plus an empty-base both-sides-add
    // emitted a duplicated opening `{` (so `{`/`}` no longer balance) while the
    // outcome was `Conflicts`, and a conflict skipped the clean floor — the
    // malformed markers shipped silently. A conflict the user resolves must
    // still be well-formed: resolving the markers to EITHER side must yield a
    // file that re-parses. If a resolution is unparseable (an unbalanced /
    // duplicated delimiter), route to the textual fallback, whose markers are
    // well-formed by construction. Part 1 (structural body delimiter) makes
    // this hold by construction; this guard keeps a future weave regression
    // from re-shipping the class silently.
    match &outcome {
        MergeOutcome::Clean(output) => {
            if !conserves_inputs(output, language, &base_parsed, &ours_parsed, &theirs_parsed) {
                return text_hunk_merge_with_markers(base, ours, theirs, markers);
            }
        }
        MergeOutcome::Conflicts {
            merged_bytes_with_markers,
            ..
        } => {
            if !conflict_well_formed(merged_bytes_with_markers, language) {
                return text_hunk_merge_with_markers(base, ours, theirs, markers);
            }
        }
        MergeOutcome::Binary | MergeOutcome::DeleteVsModify => {}
    }
    outcome
}

/// Whether a conflict output is structurally well-formed: resolving its markers
/// to EITHER side independently yields a file that re-parses.
///
/// Reuses the clean floor's re-parse signal ([`ParsedFile::parse`] returns
/// `None` on a tree with errors) so both floors close the SAME class with the
/// same mechanism. The duplicate-delimiter corruption (heddle#490 r6) leaves a
/// resolved side with an unbalanced `{`/`}`, which fails to parse and is caught
/// here. If the markers themselves are malformed (unbalanced
/// `<<<<<<< / ======= / >>>>>>>` nesting) the resolver returns `None`, which is
/// likewise treated as not-well-formed.
fn conflict_well_formed(output: &[u8], language: Language) -> bool {
    let Ok(text) = std::str::from_utf8(output) else {
        return false;
    };
    let Some((ours, theirs)) = resolve_conflict_sides(text) else {
        return false;
    };
    ParsedFile::parse(ours.as_str(), language).is_some()
        && ParsedFile::parse(theirs.as_str(), language).is_some()
}

/// Resolve conflict-marked `text` into its two independent sides: the
/// take-ours resolution (drop the `theirs` hunks + markers) and the take-theirs
/// resolution (drop the `ours` hunks + markers). Mirrors the marker shape
/// emitted by [`merge::markers`] / `reconstruct::emit_addadd_conflict`:
/// `<<<<<<< <label>` … `=======` … `>>>>>>> <label>`.
///
/// Returns `None` when the markers are malformed — a `=======` outside an open
/// `<<<<<<<`, a `>>>>>>>` outside an open `=======`, a nested `<<<<<<<`, or an
/// unterminated block at end of input — so a structurally broken conflict is
/// itself surfaced as not-well-formed.
fn resolve_conflict_sides(text: &str) -> Option<(String, String)> {
    enum State {
        Normal,
        Ours,
        Theirs,
    }
    let mut ours = String::new();
    let mut theirs = String::new();
    let mut state = State::Normal;
    for line in text.split_inclusive('\n') {
        let marker = conflict_marker(line);
        if matches!(marker, Some(ConflictMarker::Start)) {
            match state {
                State::Normal => state = State::Ours,
                _ => return None,
            }
        } else if matches!(marker, Some(ConflictMarker::Separator)) {
            match state {
                State::Ours => state = State::Theirs,
                _ => return None,
            }
        } else if matches!(marker, Some(ConflictMarker::End)) {
            match state {
                State::Theirs => state = State::Normal,
                _ => return None,
            }
        } else {
            match state {
                State::Normal => {
                    ours.push_str(line);
                    theirs.push_str(line);
                }
                State::Ours => ours.push_str(line),
                State::Theirs => theirs.push_str(line),
            }
        }
    }
    match state {
        State::Normal => Some((ours, theirs)),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConflictMarker {
    Start,
    Separator,
    End,
}

fn conflict_marker(line: &str) -> Option<ConflictMarker> {
    let body = line.strip_suffix('\n').unwrap_or(line);
    let body = body.strip_suffix('\r').unwrap_or(body).trim_start();

    if marker_body_matches(body, "<<<<<<<") {
        Some(ConflictMarker::Start)
    } else if marker_body_matches(body, "=======") {
        Some(ConflictMarker::Separator)
    } else if marker_body_matches(body, ">>>>>>>") {
        Some(ConflictMarker::End)
    } else {
        None
    }
}

fn marker_body_matches(body: &str, marker: &str) -> bool {
    let Some(rest) = body.strip_prefix(marker) else {
        return false;
    };
    rest.is_empty() || rest.starts_with(' ')
}

/// Whether a clean `output` conserves the structure of its inputs. Re-parses
/// the output and checks, against the three inputs (re-segmented raw so `use`
/// keys compare on the same footing as the output's):
///
/// 1. **Re-parse** — a clean merge that yields an unparseable file is a
///    corruption (catches a collapse's unbalanced delimiters).
/// 2. **Item-identity subset** — every `(scope, kind, name)` in the output
///    must appear in some input; the merge may not invent an item or move one
///    into a scope no contributing side gave it (catches mis-nesting / a child
///    escaping its container).
///
/// Both checks are deletion-safe (a subset relation, not equality), so a
/// legitimate clean merge with deletions passes; and edit-safe (they key on
/// item identity, not line text), so a within-line edit that recombines bytes
/// passes. The line-duplication class the harness pins (Bug 1's doubled
/// postamble) is excluded by construction in the tree model and covered by the
/// ported conformance tests, so it needs no production line-budget check.
fn conserves_inputs(
    output: &[u8],
    language: Language,
    base_parsed: &ParsedFile,
    ours_parsed: &ParsedFile,
    theirs_parsed: &ParsedFile,
) -> bool {
    use std::collections::BTreeSet;

    let Ok(out_text) = std::str::from_utf8(output) else {
        return false;
    };
    let Some(out_parsed) = ParsedFile::parse(out_text, language) else {
        return false;
    };

    type Identity = (Vec<String>, items::ItemKind, String);
    let collect = |seg: &items::FileSegments, set: &mut BTreeSet<Identity>| {
        items::visit_items(&seg.items, &mut |i| {
            set.insert((i.key.scope.clone(), i.key.kind, i.key.name.clone()));
        });
    };

    let mut allowed: BTreeSet<Identity> = BTreeSet::new();
    for parsed in [base_parsed, ours_parsed, theirs_parsed] {
        collect(&segment_file(parsed), &mut allowed);
    }
    let mut got: BTreeSet<Identity> = BTreeSet::new();
    collect(&segment_file(&out_parsed), &mut got);
    got.is_subset(&allowed)
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
