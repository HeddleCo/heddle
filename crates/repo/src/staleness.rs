// SPDX-License-Identifier: Apache-2.0
//! Staleness detection for code annotations.
//!
//! Checks whether annotations are still current against the latest code by
//! comparing stored source hashes against the current content at the annotated scope.

use std::{collections::HashMap, path::Path};

pub use objects::object::{
    StalenessStatus, annotation_status_for_source, extract_line_range, resolve_current_symbol,
};
#[cfg(feature = "async-source")]
use objects::store::AsyncObjectSource;
use objects::{
    object::{
        Annotation, Blob, ContextTarget, State, Tree,
        annotation_status_for_source_with_symbol_resolver,
    },
    store::ObjectSource,
};

use crate::Repository;

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
    if annotation
        .current_revision()
        .and_then(|revision| revision.source_hash.as_ref())
        .is_none()
    {
        return Ok(StalenessStatus::Unknown);
    }

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
    Ok(annotation_status_for_source_with_symbol_resolver(
        annotation,
        &annotation.scope,
        source,
        file_path,
        resolve_current_symbol_for_repo,
    ))
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
    resolve_current_symbol_for_repo(blob.content(), file_path, symbol, None)
}

/// Internal: tree-sitter resolution with a fallback. When the feature is
/// enabled, `Ok` returns the live range; `SymbolNotFound` returns `None`;
/// language/parse errors fall back to `stored`. When the feature is
/// disabled, simply return `stored`.
#[cfg(feature = "tree-sitter-symbols")]
fn resolve_current_symbol_for_repo(
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
fn resolve_current_symbol_for_repo(
    _source: &[u8],
    _file_path: &std::path::Path,
    _symbol: &str,
    stored: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    stored
}

/// Resolve a blob at a file path within a tree by walking the tree hierarchy.
fn get_blob_at_path(
    store: &impl ObjectSource,
    tree: &Tree,
    path: &str,
) -> Result<Option<Blob>, anyhow::Error> {
    let parts: Vec<&str> = path.split('/').collect();
    get_blob_recursive(store, tree, &parts)
}

fn get_blob_recursive(
    store: &impl ObjectSource,
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
        // Serve symlink entries too: a symlink's object is a blob whose bytes
        // are the target path. We return that target-path content (git
        // semantics), NOT the content of the file it points at — following the
        // link is a higher-level fs concern. `is_symlink()` lets such callers
        // opt into following it themselves.
        if let Some(blob_hash) = entry.leaf_content_hash() {
            return Ok(store.get_blob(&blob_hash)?);
        }
    } else if let Some(tree_hash) = entry.tree_hash()
        && let Some(subtree) = store.get_tree(&tree_hash)?
    {
        return get_blob_recursive(store, &subtree, &parts[1..]);
    }

    Ok(None)
}

/// Resolve a blob at a file path within a tree by walking the tree hierarchy.
#[cfg(feature = "async-source")]
pub async fn get_blob_at_path_async<S>(
    store: &S,
    tree: &Tree,
    path: &str,
) -> Result<Option<Blob>, anyhow::Error>
where
    S: AsyncObjectSource + Sync + ?Sized,
{
    let parts: Vec<&str> = path.split('/').collect();
    get_blob_recursive_async(store, tree, &parts).await
}

#[cfg(feature = "async-source")]
async fn get_blob_recursive_async<S>(
    store: &S,
    tree: &Tree,
    parts: &[&str],
) -> Result<Option<Blob>, anyhow::Error>
where
    S: AsyncObjectSource + Sync + ?Sized,
{
    if parts.is_empty() {
        return Ok(None);
    }

    let name = parts[0];
    let entry = match tree.get(name) {
        Some(e) => e,
        None => return Ok(None),
    };

    if parts.len() == 1 {
        // See `get_blob_recursive`: symlink entries resolve to their blob
        // (the target-path bytes), git-style; we do not follow the link.
        if let Some(blob_hash) = entry.leaf_content_hash() {
            return Ok(store.get_blob(&blob_hash).await?);
        }
    } else if let Some(tree_hash) = entry.tree_hash()
        && let Some(subtree) = store.get_tree(&tree_hash).await?
    {
        return Box::pin(get_blob_recursive_async(store, &subtree, &parts[1..])).await;
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use objects::{
        object::{AnnotationScope, ContentHash},
        store::ObjectStore,
    };
    #[cfg(feature = "async-source")]
    use objects::{
        object::{
            Blob, ChangeId, DiffKind, EntryType, FileChange, TreeEntry, diff_trees_visit,
            diff_trees_visit_async,
        },
        store::{AsyncObjectSource, InMemoryStore},
    };

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

    #[cfg(feature = "async-source")]
    struct AsyncInMemorySource<'a>(&'a InMemoryStore);

    #[cfg(feature = "async-source")]
    impl AsyncObjectSource for AsyncInMemorySource<'_> {
        async fn get_tree(&self, hash: &ContentHash) -> objects::error::Result<Option<Tree>> {
            ObjectStore::get_tree(self.0, hash)
        }

        async fn get_state(&self, id: &ChangeId) -> objects::error::Result<Option<State>> {
            ObjectStore::get_state(self.0, id)
        }

        async fn get_blob(&self, hash: &ContentHash) -> objects::error::Result<Option<Blob>> {
            ObjectStore::get_blob(self.0, hash)
        }
    }

    #[cfg(feature = "async-source")]
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        use std::task::{Context, Poll, Waker};

        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);

        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(output) => return output,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[cfg(feature = "async-source")]
    fn create_blob(store: &InMemoryStore, content: &[u8]) -> ContentHash {
        ObjectStore::put_blob(store, &Blob::from_slice(content)).unwrap()
    }

    #[cfg(feature = "async-source")]
    fn create_tree(
        store: &InMemoryStore,
        entries: Vec<(&str, ContentHash, EntryType)>,
    ) -> ContentHash {
        let entries = entries
            .into_iter()
            .map(|(name, hash, entry_type)| match entry_type {
                EntryType::Blob => TreeEntry::file(name.to_string(), hash, false),
                EntryType::Tree => TreeEntry::directory(name.to_string(), hash),
                EntryType::Symlink => TreeEntry::symlink(name.to_string(), hash),
                EntryType::Gitlink => unreachable!("staleness tests do not build gitlinks"),
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        ObjectStore::put_tree(store, &Tree::from_entries(entries)).unwrap()
    }

    #[cfg(feature = "async-source")]
    fn blob_content(blob: Option<Blob>) -> Option<Vec<u8>> {
        blob.map(Blob::into_content)
    }

    #[cfg(feature = "async-source")]
    #[test]
    fn async_source_golden_vectors_match_sync_walkers() {
        let store = InMemoryStore::new();
        let async_store = AsyncInMemorySource(&store);

        let from_nested = create_blob(&store, b"old nested");
        let from_nested_tree = create_tree(&store, vec![("c.txt", from_nested, EntryType::Blob)]);
        let to_nested = create_blob(&store, b"new nested");
        let to_nested_tree = create_tree(&store, vec![("b.txt", to_nested, EntryType::Blob)]);

        let from_hash = create_tree(
            &store,
            vec![
                ("a.txt", create_blob(&store, b"old a"), EntryType::Blob),
                ("dir", from_nested_tree, EntryType::Tree),
                ("same.txt", create_blob(&store, b"same"), EntryType::Blob),
                ("z.txt", create_blob(&store, b"old z"), EntryType::Blob),
            ],
        );
        let to_hash = create_tree(
            &store,
            vec![
                ("b.txt", create_blob(&store, b"new b"), EntryType::Blob),
                ("dir", to_nested_tree, EntryType::Tree),
                ("same.txt", create_blob(&store, b"same"), EntryType::Blob),
                ("z.txt", create_blob(&store, b"new z"), EntryType::Blob),
            ],
        );

        let expected_changes = vec![
            ("a.txt".to_string(), DiffKind::Deleted),
            ("b.txt".to_string(), DiffKind::Added),
            ("dir/b.txt".to_string(), DiffKind::Added),
            ("dir/c.txt".to_string(), DiffKind::Deleted),
            ("z.txt".to_string(), DiffKind::Modified),
        ];

        let mut sync_changes = Vec::new();
        let _ = diff_trees_visit(&store, &from_hash, &to_hash, |change| {
            sync_changes.push(FileChange::into_tuple(change));
            std::ops::ControlFlow::<()>::Continue(())
        })
        .unwrap();

        let mut async_changes = Vec::new();
        let _ = block_on(diff_trees_visit_async(
            &async_store,
            &from_hash,
            &to_hash,
            |change| {
                async_changes.push(FileChange::into_tuple(change));
                std::ops::ControlFlow::<()>::Continue(())
            },
        ))
        .unwrap();

        assert_eq!(sync_changes, expected_changes);
        assert_eq!(async_changes, expected_changes);
        assert_eq!(async_changes, sync_changes);

        let to_tree = ObjectStore::get_tree(&store, &to_hash).unwrap().unwrap();
        let path_vectors = [
            ("b.txt", Some(b"new b".to_vec())),
            ("dir/b.txt", Some(b"new nested".to_vec())),
            ("dir/c.txt", None),
        ];

        for (path, expected_blob) in path_vectors {
            let sync_blob = blob_content(get_blob_at_path(&store, &to_tree, path).unwrap());
            let async_blob = blob_content(
                block_on(get_blob_at_path_async(&async_store, &to_tree, path)).unwrap(),
            );
            assert_eq!(sync_blob, expected_blob, "sync path {path}");
            assert_eq!(async_blob, expected_blob, "async path {path}");
            assert_eq!(async_blob, sync_blob, "dual path {path}");
        }
    }

    #[cfg(feature = "async-source")]
    #[test]
    fn symlink_entry_resolves_to_target_path_bytes() {
        let store = InMemoryStore::new();
        let async_store = AsyncInMemorySource(&store);

        // A symlink's object *is* a blob whose bytes are the target path.
        let link_target = create_blob(&store, b"AGENTS.md");
        let real_file = create_blob(&store, b"the real content");
        let subdir_file = create_blob(&store, b"nested content");

        let subdir = create_tree(&store, vec![("inner.txt", subdir_file, EntryType::Blob)]);

        let root_hash = create_tree(
            &store,
            vec![
                ("AGENTS.md", real_file, EntryType::Blob),
                ("CLAUDE.md", link_target, EntryType::Symlink),
                ("dir", subdir, EntryType::Tree),
            ],
        );
        let root = ObjectStore::get_tree(&store, &root_hash).unwrap().unwrap();

        // The symlink resolves to its blob (the target-path bytes), NOT followed
        // to the target file's content, and NOT dropped as not-found.
        let sync_symlink = blob_content(get_blob_at_path(&store, &root, "CLAUDE.md").unwrap());
        let async_symlink = blob_content(
            block_on(get_blob_at_path_async(&async_store, &root, "CLAUDE.md")).unwrap(),
        );
        assert_eq!(sync_symlink, Some(b"AGENTS.md".to_vec()), "sync symlink");
        assert_eq!(async_symlink, Some(b"AGENTS.md".to_vec()), "async symlink");

        // Regression: a regular blob still resolves to its own content.
        let sync_blob = blob_content(get_blob_at_path(&store, &root, "AGENTS.md").unwrap());
        let async_blob = blob_content(
            block_on(get_blob_at_path_async(&async_store, &root, "AGENTS.md")).unwrap(),
        );
        assert_eq!(sync_blob, Some(b"the real content".to_vec()), "sync blob");
        assert_eq!(async_blob, Some(b"the real content".to_vec()), "async blob");

        // Regression: a directory does not resolve as a blob.
        let sync_dir = blob_content(get_blob_at_path(&store, &root, "dir").unwrap());
        let async_dir =
            blob_content(block_on(get_blob_at_path_async(&async_store, &root, "dir")).unwrap());
        assert_eq!(sync_dir, None, "sync dir");
        assert_eq!(async_dir, None, "async dir");
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
    fn annotation_status_for_source_matches_legacy_inline_tail() {
        let source = b"fn kept() {\n    println!(\"kept\");\n}\n\nfn other() {}\n";
        let file_path = Path::new("src/lib.rs");
        let file_hash = ContentHash::compute(source);
        let lines_hash = ContentHash::compute(&extract_line_range(source, 1, 3));
        let symbol_hash = ContentHash::compute(&extract_line_range(source, 1, 3));
        let changed_hash = ContentHash::compute(b"old bytes");

        let cases = vec![
            make_annotation(AnnotationScope::File, Some(file_hash)),
            make_annotation(AnnotationScope::Lines(1, 3), Some(lines_hash)),
            make_annotation(
                AnnotationScope::Symbol {
                    name: "kept".to_string(),
                    resolved_lines: Some((1, 3)),
                },
                Some(symbol_hash),
            ),
            make_annotation(
                AnnotationScope::Symbol {
                    name: "missing".to_string(),
                    resolved_lines: None,
                },
                Some(changed_hash),
            ),
            make_annotation(AnnotationScope::File, None),
        ];

        for annotation in cases {
            let legacy = legacy_annotation_status_for_source(&annotation, source, file_path);
            let lifted =
                annotation_status_for_source(&annotation, &annotation.scope, source, file_path);
            assert_eq!(lifted, legacy, "scope {}", annotation.scope);
        }
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

    fn legacy_annotation_status_for_source(
        annotation: &Annotation,
        source: &[u8],
        file_path: &Path,
    ) -> StalenessStatus {
        let Some(revision) = annotation.current_revision() else {
            return StalenessStatus::Unknown;
        };
        let expected_hash = match &revision.source_hash {
            Some(h) => h,
            None => return StalenessStatus::Unknown,
        };

        let scoped_bytes = match &annotation.scope {
            AnnotationScope::File => source.to_vec(),
            AnnotationScope::Lines(start, end) => extract_line_range(source, *start, *end),
            AnnotationScope::Symbol {
                name,
                resolved_lines,
            } => match resolve_current_symbol_for_repo(source, file_path, name, *resolved_lines) {
                Some((start, end)) => extract_line_range(source, start, end),
                None => {
                    return StalenessStatus::SymbolMissing {
                        symbol: name.clone(),
                    };
                }
            },
        };

        let current_hash = ContentHash::compute(&scoped_bytes);
        if current_hash == *expected_hash {
            StalenessStatus::Fresh
        } else {
            StalenessStatus::SourceChanged {
                old_hash: *expected_hash,
                new_hash: current_hash,
            }
        }
    }

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
