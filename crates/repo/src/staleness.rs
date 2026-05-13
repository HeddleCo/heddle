// SPDX-License-Identifier: Apache-2.0
//! Staleness detection for code annotations.
//!
//! Checks whether annotations are still current against the latest code by
//! comparing stored source hashes against the current content at the annotated scope.

use std::{collections::HashMap, path::Path};

use objects::{
    object::{Annotation, AnnotationScope, Blob, ContentHash, ContextTarget, State, Tree},
    store::ObjectStore,
};

use crate::Repository;

/// Result of checking an annotation's freshness against current code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StalenessStatus {
    /// Source hash matches -- annotation is current.
    Fresh,
    /// Source at the annotated scope has changed since the annotation was written.
    SourceChanged {
        old_hash: ContentHash,
        new_hash: ContentHash,
    },
    /// The file referenced by the annotation no longer exists in the tree.
    FileMissing,
    /// Symbol referenced by annotation no longer exists in the file.
    SymbolMissing { symbol: String },
    /// No provenance data stored -- staleness cannot be determined.
    Unknown,
}

/// Check a single annotation's staleness against current code.
///
/// Compares the annotation's stored `source_hash` against the BLAKE3 hash of the
/// current content at the annotation's scope within the state's code tree.
pub fn check_annotation_staleness(
    repo: &Repository,
    annotation: &Annotation,
    target: &ContextTarget,
    current_state: &State,
) -> Result<StalenessStatus, anyhow::Error> {
    let Some(file_path) = target.path() else {
        return Ok(StalenessStatus::Unknown);
    };
    let file_path = Path::new(file_path);
    let Some(revision) = annotation.current_revision() else {
        return Ok(StalenessStatus::Unknown);
    };
    // If no source_hash stored, annotation predates provenance tracking.
    let expected_hash = match &revision.source_hash {
        Some(h) => h,
        None => return Ok(StalenessStatus::Unknown),
    };

    // Load the current code tree.
    let tree = match repo.store().get_tree(&current_state.tree)? {
        Some(t) => t,
        None => return Ok(StalenessStatus::FileMissing),
    };

    // Resolve file content from the tree.
    let path_str = file_path.to_string_lossy();
    let blob = match get_blob_at_path(repo.store(), &tree, &path_str)? {
        Some(b) => b,
        None => return Ok(StalenessStatus::FileMissing),
    };

    let source = blob.content();

    // Extract the bytes at the annotation's scope.
    let scoped_bytes = match &annotation.scope {
        AnnotationScope::File => source.to_vec(),
        AnnotationScope::Lines(start, end) => extract_line_range(source, *start, *end),
        AnnotationScope::Symbol {
            name,
            resolved_lines,
        } => {
            // Re-resolve the symbol against the current tree when tree-
            // sitter is available: this tracks symbols that moved within
            // the file (e.g. an unrelated function was added above).
            //   - resolver succeeds → use the current line range (handles
            //     "symbol moved but body unchanged" as Fresh).
            //   - resolver reports SymbolNotFound → report SymbolMissing
            //     even if we have stale `resolved_lines` from creation time.
            //   - resolver errors for parse/language reasons → fall back to
            //     stored `resolved_lines` (best effort).
            // When the feature is disabled, fall back to the stored range.
            let current_lines = resolve_current_symbol(source, file_path, name, *resolved_lines);
            match current_lines {
                Some((start, end)) => extract_line_range(source, start, end),
                None => {
                    return Ok(StalenessStatus::SymbolMissing {
                        symbol: name.clone(),
                    });
                }
            }
        }
    };

    // BLAKE3 hash the current bytes and compare with expected.
    let current_hash = ContentHash::compute(&scoped_bytes);
    if current_hash == *expected_hash {
        Ok(StalenessStatus::Fresh)
    } else {
        Ok(StalenessStatus::SourceChanged {
            old_hash: *expected_hash,
            new_hash: current_hash,
        })
    }
}

/// Extract bytes for a line range from source content.
///
/// Lines are 1-indexed. Returns the bytes spanning `start..=end` lines
/// (inclusive on both ends), joined with newlines.
pub fn extract_line_range(source: &[u8], start: u32, end: u32) -> Vec<u8> {
    let text = std::str::from_utf8(source).unwrap_or("");
    let lines: Vec<&str> = text.lines().collect();
    let start_idx = (start as usize).saturating_sub(1);
    let end_idx = (end as usize).min(lines.len());
    if start_idx >= end_idx {
        return Vec::new();
    }
    lines[start_idx..end_idx].join("\n").into_bytes()
}

/// Batch check: check staleness for all annotations across all files in a context tree.
///
/// Returns a map keyed by `"{file_path}:{scope}"` to `StalenessStatus`.
pub fn check_context_staleness(
    repo: &Repository,
    current_state: &State,
) -> Result<HashMap<String, StalenessStatus>, anyhow::Error> {
    let mut results = HashMap::new();

    let context_root = match &current_state.context {
        Some(root) => root,
        None => return Ok(results),
    };

    let entries = repo.list_context_entries(context_root, None)?;

    for entry in &entries {
        for annotation in &entry.blob.annotations {
            let status =
                check_annotation_staleness(repo, annotation, &entry.target, current_state)?;
            let key = match &entry.target {
                ContextTarget::File { path } => format!("{path}:{}", annotation.scope),
                ContextTarget::State { change_id } => {
                    format!(
                        "state:{}:{}",
                        change_id.to_string_full(),
                        annotation.annotation_id
                    )
                }
            };
            results.insert(key, status);
        }
    }

    Ok(results)
}

/// Re-resolve a symbol against the current code tree at `state` and return
/// the live `(start, end)` line range, or `None` when the symbol cannot be
/// located (file missing, language unsupported, parse failure, or symbol
/// genuinely absent). Cheap when callers already loaded the source — but
/// here we re-read because callers don't always hold the bytes.
pub fn live_symbol_lines(
    repo: &Repository,
    state: &State,
    path: &str,
    symbol: &str,
) -> Option<(u32, u32)> {
    let tree = repo.store().get_tree(&state.tree).ok().flatten()?;
    let blob = get_blob_at_path(repo.store(), &tree, path).ok().flatten()?;
    let file_path = std::path::Path::new(path);
    resolve_current_symbol(blob.content(), file_path, symbol, None)
}

/// Internal: tree-sitter resolution with a fallback. When the feature is
/// enabled, `Ok` returns the live range; `SymbolNotFound` returns `None`;
/// language/parse errors fall back to `stored`. When the feature is
/// disabled, simply return `stored`.
#[cfg(feature = "tree-sitter-symbols")]
fn resolve_current_symbol(
    source: &[u8],
    file_path: &std::path::Path,
    symbol: &str,
    stored: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    use crate::symbol_resolver::{SymbolResolveError, resolve_symbol_lines};
    match resolve_symbol_lines(source, file_path, symbol) {
        Ok(range) => Some(range),
        Err(SymbolResolveError::SymbolNotFound(_)) => None,
        Err(SymbolResolveError::UnsupportedLanguage(_)) | Err(SymbolResolveError::ParseFailed) => {
            stored
        }
    }
}

#[cfg(not(feature = "tree-sitter-symbols"))]
fn resolve_current_symbol(
    _source: &[u8],
    _file_path: &std::path::Path,
    _symbol: &str,
    stored: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    stored
}

/// Resolve a blob at a file path within a tree by walking the tree hierarchy.
fn get_blob_at_path(
    store: &dyn ObjectStore,
    tree: &Tree,
    path: &str,
) -> Result<Option<Blob>, anyhow::Error> {
    let parts: Vec<&str> = path.split('/').collect();
    get_blob_recursive(store, tree, &parts)
}

fn get_blob_recursive(
    store: &dyn ObjectStore,
    tree: &Tree,
    parts: &[&str],
) -> Result<Option<Blob>, anyhow::Error> {
    if parts.is_empty() {
        return Ok(None);
    }

    let name = parts[0];
    let entry = match tree.get(name) {
        Some(e) => e,
        None => return Ok(None),
    };

    if parts.len() == 1 {
        if entry.is_blob() {
            return Ok(store.get_blob(&entry.hash)?);
        }
    } else if entry.is_tree()
        && let Some(subtree) = store.get_tree(&entry.hash)?
    {
        return get_blob_recursive(store, &subtree, &parts[1..]);
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_annotation(scope: AnnotationScope, source_hash: Option<ContentHash>) -> Annotation {
        Annotation::new(
            scope,
            objects::object::AnnotationKind::Rationale,
            "test".to_string(),
            vec![],
            "test@example.com".to_string(),
            1700000000,
            source_hash,
            None,
        )
    }

    #[test]
    fn extract_line_range_basic() {
        let source = b"line1\nline2\nline3\nline4\nline5";
        // Lines 2..=4
        let result = extract_line_range(source, 2, 4);
        assert_eq!(result, b"line2\nline3\nline4");
    }

    #[test]
    fn extract_line_range_single_line() {
        let source = b"alpha\nbeta\ngamma";
        let result = extract_line_range(source, 2, 2);
        assert_eq!(result, b"beta");
    }

    #[test]
    fn extract_line_range_entire_file() {
        let source = b"a\nb\nc";
        let result = extract_line_range(source, 1, 3);
        assert_eq!(result, b"a\nb\nc");
    }

    #[test]
    fn extract_line_range_beyond_end() {
        let source = b"only\ntwo";
        // Request lines 1..=10 but only 2 lines exist
        let result = extract_line_range(source, 1, 10);
        assert_eq!(result, b"only\ntwo");
    }

    #[test]
    fn extract_line_range_empty_when_start_exceeds_end() {
        let source = b"line1\nline2";
        let result = extract_line_range(source, 5, 3);
        assert!(result.is_empty());
    }

    #[test]
    fn extract_line_range_empty_source() {
        let result = extract_line_range(b"", 1, 5);
        assert!(result.is_empty());
    }

    #[test]
    fn staleness_unknown_without_source_hash() {
        let annotation = make_annotation(AnnotationScope::File, None);

        let (_dir, repo) = make_test_repo();
        let state = make_empty_state();
        let target = ContextTarget::file("src/main.rs").unwrap();

        let status = check_annotation_staleness(&repo, &annotation, &target, &state).unwrap();

        assert_eq!(status, StalenessStatus::Unknown);
    }

    #[test]
    fn staleness_fresh_when_hash_matches() {
        let (_dir, repo) = make_test_repo();

        // Store a file blob in the tree.
        let file_content = b"fn main() { println!(\"hello\"); }";
        let blob = objects::object::Blob::new(file_content.to_vec());
        let blob_hash = repo.store().put_blob(&blob).unwrap();

        let entry = objects::object::TreeEntry::file("main.rs", blob_hash, false).unwrap();
        let tree = objects::object::Tree::from_entries(vec![entry]);
        let tree_hash = repo.store().put_tree(&tree).unwrap();

        let source_hash = ContentHash::compute(file_content);
        let annotation = make_annotation(AnnotationScope::File, Some(source_hash));

        let state = make_state_with_tree(tree_hash);
        let target = ContextTarget::file("main.rs").unwrap();

        let status = check_annotation_staleness(&repo, &annotation, &target, &state).unwrap();

        assert_eq!(status, StalenessStatus::Fresh);
    }

    #[test]
    fn staleness_source_changed_when_hash_differs() {
        let (_dir, repo) = make_test_repo();

        let new_content = b"fn main() { println!(\"updated\"); }";
        let blob = objects::object::Blob::new(new_content.to_vec());
        let blob_hash = repo.store().put_blob(&blob).unwrap();

        let entry = objects::object::TreeEntry::file("main.rs", blob_hash, false).unwrap();
        let tree = objects::object::Tree::from_entries(vec![entry]);
        let tree_hash = repo.store().put_tree(&tree).unwrap();

        let old_content = b"fn main() { println!(\"hello\"); }";
        let old_hash = ContentHash::compute(old_content);
        let annotation = make_annotation(AnnotationScope::File, Some(old_hash));

        let state = make_state_with_tree(tree_hash);
        let target = ContextTarget::file("main.rs").unwrap();

        let status = check_annotation_staleness(&repo, &annotation, &target, &state).unwrap();

        match status {
            StalenessStatus::SourceChanged {
                old_hash: got_old,
                new_hash: got_new,
            } => {
                assert_eq!(got_old, old_hash);
                assert_eq!(got_new, ContentHash::compute(new_content));
            }
            other => panic!("expected SourceChanged, got {:?}", other),
        }
    }

    #[test]
    fn staleness_file_missing() {
        let (_dir, repo) = make_test_repo();

        let tree = objects::object::Tree::new();
        let tree_hash = repo.store().put_tree(&tree).unwrap();

        let annotation = make_annotation(
            AnnotationScope::File,
            Some(ContentHash::compute(b"anything")),
        );

        let state = make_state_with_tree(tree_hash);
        let target = ContextTarget::file("missing.rs").unwrap();

        let status = check_annotation_staleness(&repo, &annotation, &target, &state).unwrap();

        assert_eq!(status, StalenessStatus::FileMissing);
    }

    #[test]
    fn staleness_lines_scope_fresh() {
        let (_dir, repo) = make_test_repo();

        let file_content = b"line1\nline2\nline3\nline4\nline5";
        let blob = objects::object::Blob::new(file_content.to_vec());
        let blob_hash = repo.store().put_blob(&blob).unwrap();

        let entry = objects::object::TreeEntry::file("file.rs", blob_hash, false).unwrap();
        let tree = objects::object::Tree::from_entries(vec![entry]);
        let tree_hash = repo.store().put_tree(&tree).unwrap();

        let lines_content = extract_line_range(file_content, 2, 3);
        let source_hash = ContentHash::compute(&lines_content);

        let annotation = make_annotation(AnnotationScope::Lines(2, 3), Some(source_hash));

        let state = make_state_with_tree(tree_hash);
        let target = ContextTarget::file("file.rs").unwrap();

        let status = check_annotation_staleness(&repo, &annotation, &target, &state).unwrap();

        assert_eq!(status, StalenessStatus::Fresh);
    }

    /// Symbol-scope annotations whose source has shifted line-wise but not
    /// content-wise should report Fresh: tree-sitter re-resolves the new
    /// position and the hash of those bytes still matches what we stored.
    /// Without re-resolution this would falsely report SourceChanged.
    #[cfg(feature = "tree-sitter-symbols")]
    #[test]
    fn staleness_symbol_scope_tracks_moved_symbol() {
        let (_dir, repo) = make_test_repo();

        // Original file: `insert` at lines 1-3.
        let original = b"export function insert() {\n  return 1;\n}\n";
        // Same `insert` body, but shifted down by adding two unrelated
        // lines above it (so it now lives at lines 3-5).
        let shifted =
            b"// Note: shifted symbol\nexport const x = 0;\nexport function insert() {\n  return 1;\n}\n";

        // Hash the bytes of the SYMBOL at original lines, exactly as
        // `compute_source_hash` would have done at annotation time.
        let original_symbol_bytes = extract_line_range(original, 1, 3);
        let source_hash = ContentHash::compute(&original_symbol_bytes);

        // Store the SHIFTED file in the tree we'll check against.
        let blob = objects::object::Blob::new(shifted.to_vec());
        let blob_hash = repo.store().put_blob(&blob).unwrap();
        let entry = objects::object::TreeEntry::file("file.ts", blob_hash, false).unwrap();
        let tree = objects::object::Tree::from_entries(vec![entry]);
        let tree_hash = repo.store().put_tree(&tree).unwrap();

        // Annotation was authored against original lines 1-3.
        let annotation = make_annotation(
            AnnotationScope::Symbol {
                name: "insert".to_string(),
                resolved_lines: Some((1, 3)),
            },
            Some(source_hash),
        );

        let state = make_state_with_tree(tree_hash);
        let target = ContextTarget::file("file.ts").unwrap();

        let status = check_annotation_staleness(&repo, &annotation, &target, &state).unwrap();
        assert_eq!(status, StalenessStatus::Fresh);
    }

    /// When the symbol genuinely no longer exists in the file, return
    /// SymbolMissing — even if the annotation's stored `resolved_lines`
    /// happens to point at unrelated content that lines up at those rows.
    #[cfg(feature = "tree-sitter-symbols")]
    #[test]
    fn staleness_symbol_scope_reports_missing_when_symbol_gone() {
        let (_dir, repo) = make_test_repo();

        let new_content = b"export const x = 1;\nexport const y = 2;\n";
        let blob = objects::object::Blob::new(new_content.to_vec());
        let blob_hash = repo.store().put_blob(&blob).unwrap();
        let entry = objects::object::TreeEntry::file("file.ts", blob_hash, false).unwrap();
        let tree = objects::object::Tree::from_entries(vec![entry]);
        let tree_hash = repo.store().put_tree(&tree).unwrap();

        let annotation = make_annotation(
            AnnotationScope::Symbol {
                name: "insert".to_string(),
                resolved_lines: Some((1, 3)),
            },
            Some(ContentHash::compute(b"whatever")),
        );

        let state = make_state_with_tree(tree_hash);
        let target = ContextTarget::file("file.ts").unwrap();

        let status = check_annotation_staleness(&repo, &annotation, &target, &state).unwrap();
        assert_eq!(
            status,
            StalenessStatus::SymbolMissing {
                symbol: "insert".to_string()
            }
        );
    }

    // -- test helpers --

    fn make_test_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::TempDir::new().unwrap();
        let repo = Repository::init_default(dir.path()).unwrap();
        (dir, repo)
    }

    fn make_empty_state() -> State {
        use objects::object::{Attribution, Principal};
        let principal = Principal::new("Test", "test@example.com");
        let attribution = Attribution::human(principal);
        State::new(ContentHash::compute(b"empty-tree"), vec![], attribution)
    }

    fn make_state_with_tree(tree_hash: ContentHash) -> State {
        use objects::object::{Attribution, Principal};
        let principal = Principal::new("Test", "test@example.com");
        let attribution = Attribution::human(principal);
        State::new(tree_hash, vec![], attribution)
    }
}