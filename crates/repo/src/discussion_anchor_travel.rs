// SPDX-License-Identifier: Apache-2.0
//! Server-side discussion anchor travel.
//!
//! When a snapshot mutates the source tree, every open discussion's
//! [`SymbolAnchor`] must be re-evaluated against the new tree before it's
//! persisted. The five cases:
//!
//! 1. **Unchanged** — anchor resolves at the same `(file, symbol)` and
//!    the body bytes are byte-identical → no update.
//! 2. **Body changed** — anchor resolves, body bytes differ →
//!    `body_changed_since_open = true`. Reviewers see a marker;
//!    resolution still proceeds normally.
//! 3. **Renamed within file** — symbol no longer resolves at the old
//!    name, but a structurally-similar definition exists in the same
//!    file. Out of scope for now: function-level rename detection
//!    requires a tree-sitter call-graph diff that hasn't been wired
//!    yet. Falls through to the cross-file path with the old file
//!    kept in scope.
//! 4. **Cross-file move** — file moved (rename detected by
//!    [`detect_file_renames`] with confidence above
//!    [`RENAME_CONFIDENCE_FOR_ANCHOR_TRAVEL`]); re-anchor to the new
//!    path, recompute body-changed.
//! 5. **Orphaned** — none of the above. The discussion stays open with
//!    `orphaned = true` for human triage; no auto-resolution.
//!
//! The function is pure (file maps in, updates out) so the snapshot path
//! can call it under the snapshot write batch without holding repo
//! locks longer than necessary, and so tests can exercise the five
//! cases without touching disk.

#![cfg(feature = "tree-sitter-symbols")]

use std::collections::HashMap;
use std::path::Path;

use objects::object::{Discussion, SymbolAnchor};
use semantic::analysis::{SimilarityMethod, detect_file_renames};
use semantic::symbol_resolver::resolve_symbol_lines;

/// Confidence threshold for accepting a file rename when re-anchoring a
/// discussion. Below this we'd rather mark the discussion `orphaned`
/// than misroute it to a coincidentally-similar new file.
pub const RENAME_CONFIDENCE_FOR_ANCHOR_TRAVEL: f32 = 0.85;

/// One anchor-travel decision for one discussion. The caller persists
/// these by mutating the corresponding [`Discussion`] in the new state's
/// `DiscussionsBlob`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscussionAnchorUpdate {
    pub discussion_id: String,
    /// Where the anchor now points. Equals the original anchor when the
    /// symbol resolved at its original `(file, symbol)`.
    pub new_anchor: SymbolAnchor,
    /// Set when the resolved body bytes differ from the body at the
    /// state the discussion was opened against.
    pub body_changed_since_open: bool,
    /// Set when no resolution path produced a hit — neither original
    /// location, nor renamed file, nor cross-file move.
    pub orphaned: bool,
}

/// Run anchor travel for `open_discussions`. `old_files` and `new_files`
/// are full-content file maps keyed by repo-relative path; the old map
/// only needs to carry the files referenced by an open discussion (plus
/// any candidates a rename detector might consider).
///
/// Returns one update per discussion in input order.
pub fn travel_anchors(
    old_files: &HashMap<String, Vec<u8>>,
    new_files: &HashMap<String, Vec<u8>>,
    open_discussions: &[Discussion],
) -> Vec<DiscussionAnchorUpdate> {
    // Pre-compute rename candidates once for the whole batch — the
    // detector is O(deleted × added) and we'd otherwise pay it per
    // discussion. Restrict to files that disappeared on this transition;
    // anchor travel only cares about an old file vanishing.
    let renamed = compute_renames(old_files, new_files);

    open_discussions
        .iter()
        .map(|d| travel_one(d, old_files, new_files, &renamed))
        .collect()
}

/// Resolve a single discussion against the new tree. Mirrors the five
/// cases above; lifted out of [`travel_anchors`] so each case is its own
/// expression and the batch loop stays readable.
fn travel_one(
    discussion: &Discussion,
    old_files: &HashMap<String, Vec<u8>>,
    new_files: &HashMap<String, Vec<u8>>,
    renamed: &HashMap<String, String>,
) -> DiscussionAnchorUpdate {
    let original = &discussion.anchor;
    let old_body = old_files.get(&original.file).and_then(|src| {
        match resolve_symbol_lines(src, Path::new(&original.file), &original.symbol) {
            Ok((start, end)) => Some(extract_body(src, start, end)),
            Err(_) => None,
        }
    });

    // Case 1/2: file present in new tree, symbol resolves there.
    if let Some(new_src) = new_files.get(&original.file)
        && let Ok((start, end)) =
            resolve_symbol_lines(new_src, Path::new(&original.file), &original.symbol)
    {
        let new_body = extract_body(new_src, start, end);
        let body_changed = match &old_body {
            Some(old) => old != &new_body,
            // Couldn't resolve in old — treat as changed so reviewers
            // see the marker. Better noisy than silently lying.
            None => true,
        };
        return DiscussionAnchorUpdate {
            discussion_id: discussion.id.clone(),
            new_anchor: original.clone(),
            body_changed_since_open: body_changed,
            orphaned: false,
        };
    }

    // Case 4: file rename. The detector returned (old_path, new_path);
    // try resolving the symbol at its original name in the new file.
    if let Some(new_path) = renamed.get(&original.file)
        && let Some(new_src) = new_files.get(new_path)
        && let Ok((start, end)) =
            resolve_symbol_lines(new_src, Path::new(new_path), &original.symbol)
    {
        let new_body = extract_body(new_src, start, end);
        let body_changed = match &old_body {
            Some(old) => old != &new_body,
            None => true,
        };
        return DiscussionAnchorUpdate {
            discussion_id: discussion.id.clone(),
            new_anchor: SymbolAnchor::new(new_path.clone(), original.symbol.clone()),
            body_changed_since_open: body_changed,
            orphaned: false,
        };
    }

    // Case 5: nothing matched. Mark orphaned and leave the original
    // anchor in place so the discussion is still addressable in
    // history.
    DiscussionAnchorUpdate {
        discussion_id: discussion.id.clone(),
        new_anchor: original.clone(),
        body_changed_since_open: false,
        orphaned: true,
    }
}

/// Build a `old_path -> new_path` lookup from the rename detector. Only
/// considers files present in `old_files` but absent in `new_files`
/// (the only candidates that need a rename to keep an anchor reachable)
/// against files added in `new_files`. Threshold matches the
/// `RENAME_CONFIDENCE_FOR_ANCHOR_TRAVEL` constant.
fn compute_renames(
    old_files: &HashMap<String, Vec<u8>>,
    new_files: &HashMap<String, Vec<u8>>,
) -> HashMap<String, String> {
    let deleted: Vec<(std::path::PathBuf, String)> = old_files
        .iter()
        .filter(|(p, _)| !new_files.contains_key(*p))
        .map(|(p, bytes)| {
            (
                std::path::PathBuf::from(p),
                String::from_utf8_lossy(bytes).into_owned(),
            )
        })
        .collect();
    let added: Vec<(std::path::PathBuf, String)> = new_files
        .iter()
        .filter(|(p, _)| !old_files.contains_key(*p))
        .map(|(p, bytes)| {
            (
                std::path::PathBuf::from(p),
                String::from_utf8_lossy(bytes).into_owned(),
            )
        })
        .collect();
    if deleted.is_empty() || added.is_empty() {
        return HashMap::new();
    }
    let renames = detect_file_renames(
        &deleted,
        &added,
        RENAME_CONFIDENCE_FOR_ANCHOR_TRAVEL as f64,
        SimilarityMethod::Tokens,
    );
    renames
        .into_iter()
        .map(|(from, to)| {
            (
                from.to_string_lossy().into_owned(),
                to.to_string_lossy().into_owned(),
            )
        })
        .collect()
}

/// Extract the bytes of `[start_line, end_line]` (1-indexed inclusive)
/// from `source`. Used to compare body bytes between the open-state and
/// the new state — anchor travel marks `body_changed_since_open` when
/// these bytes diverge.
fn extract_body(source: &[u8], start_line: u32, end_line: u32) -> Vec<u8> {
    if start_line == 0 || end_line < start_line {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut line = 1u32;
    for chunk in source.split_inclusive(|b| *b == b'\n') {
        if line >= start_line && line <= end_line {
            out.extend_from_slice(chunk);
        }
        line += 1;
        if line > end_line {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use objects::object::{ChangeId, DiscussionResolution, DiscussionTurn};

    fn discussion(id: &str, file: &str, symbol: &str) -> Discussion {
        Discussion {
            id: id.to_string(),
            anchor: SymbolAnchor::new(file, symbol),
            opened_against_state: ChangeId::from_bytes([1; 16]),
            opened_at: 1_700_000_000,
            thread_ref: None,
            turns: vec![DiscussionTurn {
                author: objects::object::Principal::new("a", "a@x"),
                body: "body".into(),
                posted_at: 1_700_000_000,
            }],
            resolution: DiscussionResolution::Open,
            body_changed_since_open: false,
            orphaned: false,
            visibility: objects::object::VisibilityTier::default(),
            resolved_annotation_id: None,
        }
    }

    /// Helper — build a single-file map from `(path, content)` pairs.
    fn files(entries: &[(&str, &str)]) -> HashMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(p, c)| (p.to_string(), c.as_bytes().to_vec()))
            .collect()
    }

    const FOO_RS: &str = "fn foo() {\n    let x = 1;\n}\n";
    const FOO_RS_BODY_CHANGED: &str = "fn foo() {\n    let x = 99;\n}\n";

    #[test]
    fn case_1_unchanged_no_marker() {
        let old = files(&[("src/lib.rs", FOO_RS)]);
        let new = files(&[("src/lib.rs", FOO_RS)]);
        let d = vec![discussion("d1", "src/lib.rs", "foo")];
        let updates = travel_anchors(&old, &new, &d);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].new_anchor.file, "src/lib.rs");
        assert!(!updates[0].body_changed_since_open);
        assert!(!updates[0].orphaned);
    }

    #[test]
    fn case_2_body_changed_marks_flag() {
        let old = files(&[("src/lib.rs", FOO_RS)]);
        let new = files(&[("src/lib.rs", FOO_RS_BODY_CHANGED)]);
        let d = vec![discussion("d1", "src/lib.rs", "foo")];
        let updates = travel_anchors(&old, &new, &d);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].new_anchor.file, "src/lib.rs");
        assert!(updates[0].body_changed_since_open);
        assert!(!updates[0].orphaned);
    }

    #[test]
    fn case_4_cross_file_move_re_anchors() {
        let old = files(&[("src/old.rs", FOO_RS)]);
        // Same content, different path: rename detector accepts.
        let new = files(&[("src/new.rs", FOO_RS)]);
        let d = vec![discussion("d1", "src/old.rs", "foo")];
        let updates = travel_anchors(&old, &new, &d);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].new_anchor.file, "src/new.rs");
        assert_eq!(updates[0].new_anchor.symbol, "foo");
        assert!(!updates[0].orphaned);
    }

    #[test]
    fn case_5_orphaned_when_file_deleted_no_rename() {
        let old = files(&[("src/lib.rs", FOO_RS)]);
        // New tree dropped src/lib.rs and added an unrelated file.
        let new = files(&[("src/other.rs", "fn bar() {}\n")]);
        let d = vec![discussion("d1", "src/lib.rs", "foo")];
        let updates = travel_anchors(&old, &new, &d);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].orphaned);
        assert_eq!(updates[0].new_anchor.file, "src/lib.rs");
    }

    #[test]
    fn case_5_orphaned_when_symbol_renamed_in_place() {
        let old = files(&[("src/lib.rs", FOO_RS)]);
        // File present, but the original symbol no longer exists. We
        // don't have function-level rename detection yet, so this falls
        // through to orphaned — better than silently following a wrong
        // symbol.
        let new = files(&[("src/lib.rs", "fn renamed() {\n    let x = 1;\n}\n")]);
        let d = vec![discussion("d1", "src/lib.rs", "foo")];
        let updates = travel_anchors(&old, &new, &d);
        assert_eq!(updates.len(), 1);
        assert!(updates[0].orphaned);
    }
}
