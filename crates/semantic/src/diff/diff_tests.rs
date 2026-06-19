// SPDX-License-Identifier: Apache-2.0
use std::{collections::BTreeMap, path::Path, process::Command, time::Instant};

use objects::{
    object::{Blob, ContentHash, EntryType, FileMode, SemanticChange, Tree, TreeEntry},
    store::{InMemoryStore, ObjectStore},
};

use super::{
    SemanticBudget, SemanticCheckStatus, SemanticDiffOptions, SemanticFallbackReason,
    semantic_check_only, semantic_diff, semantic_diff_summary, semantic_diff_with_cache,
    semantic_diff_worktree_with_cache,
};
use crate::cache::SemanticParseCache;

fn create_blob(store: &InMemoryStore, content: &str) -> ContentHash {
    let blob = Blob::from_slice(content.as_bytes());
    match store.put_blob(&blob) {
        Ok(hash) => hash,
        Err(err) => panic!("failed to store blob: {err}"),
    }
}

fn create_tree(store: &InMemoryStore, entries: Vec<(&str, ContentHash)>) -> ContentHash {
    let tree = Tree::from_entries(
        entries
            .into_iter()
            .map(|(name, hash)| TreeEntry {
                name: name.to_string(),
                mode: FileMode::Normal,
                hash,
                entry_type: EntryType::Blob,
            })
            .collect(),
    );
    match store.put_tree(&tree) {
        Ok(hash) => hash,
        Err(err) => panic!("failed to store tree: {err}"),
    }
}

fn create_owned_tree(store: &InMemoryStore, entries: Vec<(String, ContentHash)>) -> ContentHash {
    let tree = Tree::from_entries(
        entries
            .into_iter()
            .map(|(name, hash)| TreeEntry {
                name,
                mode: FileMode::Normal,
                hash,
                entry_type: EntryType::Blob,
            })
            .collect(),
    );
    match store.put_tree(&tree) {
        Ok(hash) => hash,
        Err(err) => panic!("failed to store owned tree: {err}"),
    }
}

#[test]
fn semantic_diff_options_default() {
    let opts = SemanticDiffOptions::default();
    assert!(opts.rename_threshold > 0.0 && opts.rename_threshold < 1.0);
    assert!(opts.analyze_functions);
    assert!(opts.analyze_dependencies);
}

#[test]
fn check_only_reports_clean_when_content_matches() {
    let store = InMemoryStore::new();
    let blob = create_blob(&store, "fn same() {}\n");
    let from_tree = create_tree(&store, vec![("lib.rs", blob)]);
    let to_tree = create_tree(&store, vec![("lib.rs", blob)]);

    let result = semantic_check_only(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("check_only should succeed: {err}"));

    assert_eq!(result.status, SemanticCheckStatus::NoChanges);
    assert!(result.fallback_reasons.is_empty());
}

#[test]
fn summary_reports_budget_fallback() {
    let store = InMemoryStore::new();
    let from_tree = create_tree(&store, vec![]);
    let to_tree = create_tree(
        &store,
        vec![("new.rs", create_blob(&store, "fn added() {}\n"))],
    );
    let options = SemanticDiffOptions {
        budget: SemanticBudget {
            max_changed_files: 0,
            ..SemanticBudget::default()
        },
        ..SemanticDiffOptions::default()
    };

    let result = semantic_diff_summary(&store, &from_tree, &to_tree, &options)
        .unwrap_or_else(|err| panic!("summary should succeed: {err}"));

    assert_eq!(result.fallback_reasons.len(), 1);
    assert!(matches!(
        result.fallback_reasons[0],
        SemanticFallbackReason::ChangedFileBudgetExceeded {
            limit: 0,
            actual: 1
        }
    ));
}

#[test]
fn full_diff_uses_parse_budget_fallback_reasons() {
    let store = InMemoryStore::new();
    let from_tree = create_tree(
        &store,
        vec![("large.rs", create_blob(&store, "fn before() {}\n"))],
    );
    let to_tree = create_tree(
        &store,
        vec![(
            "large.rs",
            create_blob(
                &store,
                &("fn large() {\n".to_string() + &"let x = 1;\n".repeat(16) + "}\n"),
            ),
        )],
    );
    let options = SemanticDiffOptions {
        budget: SemanticBudget {
            max_parsed_files: 0,
            ..SemanticBudget::default()
        },
        ..SemanticDiffOptions::default()
    };

    let result = semantic_diff(&store, &from_tree, &to_tree, &options)
        .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result
            .fallback_reasons
            .iter()
            .any(|reason| matches!(reason, SemanticFallbackReason::ParseBudgetExceeded { .. })),
        "fallback reasons: {:?}",
        result.fallback_reasons
    );
}

#[test]
fn full_diff_file_too_large_falls_back_to_file_level_only() {
    let store = InMemoryStore::new();
    let from_tree = create_tree(
        &store,
        vec![(
            "large.rs",
            create_blob(&store, "fn process() -> usize {\n    1\n}\n"),
        )],
    );
    let to_tree = create_tree(
        &store,
        vec![(
            "large.rs",
            create_blob(
                &store,
                "fn process() -> usize {\n    let value = 2;\n    value\n}\n",
            ),
        )],
    );
    let options = SemanticDiffOptions {
        budget: SemanticBudget {
            max_file_bytes: 8,
            ..SemanticBudget::default()
        },
        ..SemanticDiffOptions::default()
    };

    let result = semantic_diff(&store, &from_tree, &to_tree, &options)
        .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result.fallback_reasons.iter().any(|reason| matches!(
            reason,
            SemanticFallbackReason::FileTooLarge { path, limit: 8, actual }
                if path == Path::new("large.rs") && *actual > 8
        )),
        "expected FileTooLarge fallback, got: {:?}",
        result.fallback_reasons
    );
    assert_file_level_only(&result.changes, "large.rs");
}

#[test]
fn full_diff_unsupported_language_falls_back_to_file_level_only() {
    let store = InMemoryStore::new();
    let from_tree = create_tree(&store, vec![("notes.txt", create_blob(&store, "old\n"))]);
    let to_tree = create_tree(&store, vec![("notes.txt", create_blob(&store, "new\n"))]);

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result.fallback_reasons.iter().any(|reason| matches!(
            reason,
            SemanticFallbackReason::UnsupportedLanguage { path }
                if path == Path::new("notes.txt")
        )),
        "expected UnsupportedLanguage fallback, got: {:?}",
        result.fallback_reasons
    );
    assert_file_level_only(&result.changes, "notes.txt");
}

#[test]
fn full_diff_parse_failed_falls_back_to_file_level_only() {
    let store = InMemoryStore::new();
    let from_tree = create_tree(
        &store,
        vec![(
            "broken.rs",
            create_blob(&store, "fn process() -> usize {\n    1\n}\n"),
        )],
    );
    let to_tree = create_tree(
        &store,
        vec![(
            "broken.rs",
            create_blob(&store, "fn process( -> usize {\n    2\n}\n"),
        )],
    );

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result.fallback_reasons.iter().any(|reason| matches!(
            reason,
            SemanticFallbackReason::ParseFailed { path } if path == Path::new("broken.rs")
        )),
        "expected ParseFailed fallback, got: {:?}",
        result.fallback_reasons
    );
    assert_file_level_only(&result.changes, "broken.rs");
}

#[test]
fn full_diff_prefers_function_rename_over_delete_extract_and_generic_file_modified() {
    let store = InMemoryStore::new();
    let old = r#"#[test]
fn test_snapshot_uses_default_confidence_for_agent() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();
    fs::write(temp_dir.path().join("agent.txt"), "content").unwrap();
    let state = repo.snapshot(None, None).unwrap();
    assert_eq!(
        state.confidence,
        Some(repo.repo_config().defaults.confidence)
    );
}"#;
    let new = r#"#[test]
fn test_snapshot_without_confidence_records_none() {
    let temp_dir = TempDir::new().unwrap();
    Repository::init_default(temp_dir.path()).unwrap();
    fs::write(temp_dir.path().join("agent.txt"), "content").unwrap();
    let state = repo.snapshot(None, None).unwrap();
    assert_eq!(state.confidence, None);
}"#;
    let from_tree = create_tree(
        &store,
        vec![("repository_tests.rs", create_blob(&store, old))],
    );
    let to_tree = create_tree(
        &store,
        vec![("repository_tests.rs", create_blob(&store, new))],
    );

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionRenamed { old_name, new_name, .. }
                    if old_name == "test_snapshot_uses_default_confidence_for_agent"
                        && new_name == "test_snapshot_without_confidence_records_none"
            )
        }),
        "expected function rename, got: {:?}",
        result.changes
    );
    assert!(
        !result
            .changes
            .iter()
            .any(|change| matches!(change, SemanticChange::FunctionDeleted { .. })),
        "rename should not be reported as delete: {:?}",
        result.changes
    );
    assert!(
        !result.changes.iter().any(|change| matches!(
            change,
            SemanticChange::FunctionAdded { .. } | SemanticChange::FunctionExtracted { .. }
        )),
        "rename should not be reported as add/extract: {:?}",
        result.changes
    );
    assert!(
        !result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FileModified { path, .. }
                    if path == &std::path::PathBuf::from("repository_tests.rs")
            )
        }),
        "specific function-level change should suppress generic file modified: {:?}",
        result.changes
    );
}

#[test]
fn full_diff_reports_body_only_function_modified_without_generic_file_modified() {
    let store = InMemoryStore::new();
    let from_tree = create_tree(
        &store,
        vec![(
            "lib.rs",
            create_blob(&store, "fn compute(x: i32) -> i32 {\n    x + 1\n}\n"),
        )],
    );
    let to_tree = create_tree(
        &store,
        vec![(
            "lib.rs",
            create_blob(&store, "fn compute(x: i32) -> i32 {\n    x * 2\n}\n"),
        )],
    );

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result
            .changes
            .iter()
            .any(|change| matches!(change, SemanticChange::FunctionModified { name, .. } if name == "compute")),
        "expected function body modification, got: {:?}",
        result.changes
    );
    assert!(
        !result
            .changes
            .iter()
            .any(|change| matches!(change, SemanticChange::FileModified { .. })),
        "specific function body change should suppress generic file modified: {:?}",
        result.changes
    );
}

#[cfg(all(
    feature = "lang-rust",
    feature = "lang-python",
    feature = "lang-javascript",
    feature = "lang-typescript"
))]
#[test]
fn full_diff_reports_function_modifications_for_common_languages() {
    let cases = [
        (
            "lib.rs",
            "fn compute(value: i32) -> i32 {\n    value + 1\n}\n",
            "fn compute(value: i32) -> i32 {\n    value * 2\n}\n",
            "compute",
        ),
        (
            "worker.py",
            "def compute(value: int) -> int:\n    return value + 1\n",
            "def compute(value: int) -> int:\n    return value * 2\n",
            "compute",
        ),
        (
            "worker.js",
            "export const compute = (value) => {\n    return value + 1;\n};\n",
            "export const compute = (value) => {\n    return value * 2;\n};\n",
            "compute",
        ),
        (
            "worker.ts",
            "export const compute = (value: number): number => {\n    return value + 1;\n};\n",
            "export const compute = (value: number): number => {\n    return value * 2;\n};\n",
            "compute",
        ),
    ];

    for (path, old, new, function_name) in cases {
        assert_language_function_modified(path, old, new, function_name);
    }
}

#[cfg(all(
    feature = "lang-c",
    feature = "lang-cpp",
    feature = "lang-go",
    feature = "lang-java"
))]
#[test]
fn full_diff_reports_function_modifications_for_extended_languages() {
    let cases = [
        (
            "worker.go",
            "package main\n\nfunc compute(value int) int {\n    return value + 1\n}\n",
            "package main\n\nfunc compute(value int) int {\n    return value * 2\n}\n",
            "compute",
        ),
        (
            "Worker.java",
            "class Worker {\n    int compute(int value) {\n        return value + 1;\n    }\n}\n",
            "class Worker {\n    int compute(int value) {\n        return value * 2;\n    }\n}\n",
            "compute",
        ),
        (
            "worker.c",
            "int compute(int value) {\n    return value + 1;\n}\n",
            "int compute(int value) {\n    return value * 2;\n}\n",
            "compute",
        ),
        (
            "worker.cpp",
            "#include <vector>\n\nint compute(int value) {\n    return value + 1;\n}\n",
            "#include <vector>\n\nint compute(int value) {\n    return value * 2;\n}\n",
            "compute",
        ),
    ];

    for (path, old, new, function_name) in cases {
        assert_language_function_modified(path, old, new, function_name);
    }
}

#[test]
fn full_diff_parse_cache_only_parses_changed_blobs() {
    let cache = SemanticParseCache::default();
    cache.clear();
    let store = InMemoryStore::new();
    let unchanged = create_blob(&store, "fn stable() -> i32 {\n    1\n}\n");
    let from_tree = create_tree(
        &store,
        vec![
            (
                "changed.rs",
                create_blob(&store, "fn compute() -> i32 {\n    1\n}\n"),
            ),
            ("unchanged.rs", unchanged),
        ],
    );
    let to_tree = create_tree(
        &store,
        vec![
            (
                "changed.rs",
                create_blob(&store, "fn compute() -> i32 {\n    2\n}\n"),
            ),
            ("unchanged.rs", unchanged),
        ],
    );

    semantic_diff_with_cache(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
        &cache,
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    let stats = cache.stats();
    assert_eq!(
        stats.stores, 2,
        "semantic diff should parse only old/new changed blobs, not unchanged CAS-identical files: {stats:?}"
    );
}

fn assert_language_function_modified(path: &str, old: &str, new: &str, function_name: &str) {
    let store = InMemoryStore::new();
    let from_tree = create_tree(&store, vec![(path, create_blob(&store, old))]);
    let to_tree = create_tree(&store, vec![(path, create_blob(&store, new))]);

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("{path} full diff should succeed: {err}"));

    assert!(
        result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionModified { name, .. } if name == function_name
            )
        }),
        "{path} should report modified function {function_name}: {:?}",
        result.changes
    );
}

fn assert_file_level_only(changes: &[SemanticChange], path: &str) {
    let expected_path = Path::new(path);
    assert!(
        changes.iter().any(|change| matches!(
            change,
            SemanticChange::FileModified { path, .. } if path == expected_path
        )),
        "expected file-level modified entry for {path}, got: {changes:?}"
    );
    assert!(
        !changes.iter().any(|change| matches!(
            change,
            SemanticChange::FunctionAdded { .. }
                | SemanticChange::FunctionExtracted { .. }
                | SemanticChange::FunctionDeleted { .. }
                | SemanticChange::FunctionRenamed { .. }
                | SemanticChange::FunctionModified { .. }
                | SemanticChange::FunctionMoved { .. }
                | SemanticChange::SignatureChanged { .. }
                | SemanticChange::DependencyAdded { .. }
                | SemanticChange::DependencyRemoved { .. }
        )),
        "fallback should not emit AST-specific changes: {changes:?}"
    );
}

#[test]
fn full_diff_reports_same_file_function_move_without_delete_add_churn() {
    let store = InMemoryStore::new();
    let old = r#"fn alpha() -> i32 {
    1
}

fn beta() -> i32 {
    2
}
"#;
    let new = r#"fn beta() -> i32 {
    2
}

fn alpha() -> i32 {
    1
}
"#;
    let from_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, old))]);
    let to_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, new))]);

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionMoved { name, old_start_line, new_start_line, .. }
                    if name == "alpha" && *old_start_line == 0 && *new_start_line == 4
            )
        }),
        "expected moved alpha function, got: {:?}",
        result.changes
    );
    assert!(
        result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionMoved { name, old_start_line, new_start_line, .. }
                    if name == "beta" && *old_start_line == 4 && *new_start_line == 0
            )
        }),
        "expected moved beta function, got: {:?}",
        result.changes
    );
    assert!(
        !result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionDeleted { .. }
                    | SemanticChange::FunctionAdded { .. }
                    | SemanticChange::FunctionExtracted { .. }
                    | SemanticChange::FileModified { .. }
            )
        }),
        "pure moves should not report delete/add/file-modified churn: {:?}",
        result.changes
    );
}

#[test]
fn full_diff_does_not_report_line_shift_from_inserted_function_as_move() {
    let store = InMemoryStore::new();
    let old = r#"fn alpha() -> i32 {
    1
}

fn beta() -> i32 {
    2
}
"#;
    let new = r#"fn inserted() -> i32 {
    0
}

fn alpha() -> i32 {
    1
}

fn beta() -> i32 {
    2
}
"#;
    let from_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, old))]);
    let to_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, new))]);

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result
            .changes
            .iter()
            .any(|change| matches!(change, SemanticChange::FunctionAdded { name, .. } if name == "inserted")),
        "expected inserted function, got: {:?}",
        result.changes
    );
    assert!(
        !result
            .changes
            .iter()
            .any(|change| matches!(change, SemanticChange::FunctionMoved { .. })),
        "line shifts from insertions should not look like moves: {:?}",
        result.changes
    );
}

#[test]
fn full_diff_reports_function_extraction_source_when_body_moves_from_existing_function() {
    let store = InMemoryStore::new();
    let old = r#"fn checkout_total(items: &[Item]) -> u64 {
    let subtotal = items.iter().map(|item| item.price).sum::<u64>();
    let tax = subtotal / 10;
    subtotal + tax
}
"#;
    let new = r#"fn checkout_total(items: &[Item]) -> u64 {
    let subtotal = subtotal(items);
    let tax = subtotal / 10;
    subtotal + tax
}

fn subtotal(items: &[Item]) -> u64 {
    items.iter().map(|item| item.price).sum::<u64>()
}
"#;
    let from_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, old))]);
    let to_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, new))]);

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionExtracted {
                    name,
                    source_name: Some(source_name),
                    ..
                } if name == "subtotal" && source_name == "checkout_total"
            )
        }),
        "expected extraction source, got: {:?}",
        result.changes
    );
}

#[test]
fn full_diff_reports_generic_helper_body_as_added_not_extracted() {
    let store = InMemoryStore::new();
    let old = r#"fn render() -> String {
    let mut output = String::new();
    output.push_str("ready");
    output
}
"#;
    let new = r#"fn render() -> String {
    let mut output = String::new();
    output.push_str(&ready_label());
    output
}

fn ready_label() -> String {
    String::new()
}
"#;
    let from_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, old))]);
    let to_tree = create_tree(&store, vec![("lib.rs", create_blob(&store, new))]);

    let result = semantic_diff(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
    )
    .unwrap_or_else(|err| panic!("full diff should succeed: {err}"));

    assert!(
        result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionAdded {
                    name,
                    ..
                } if name == "ready_label"
            )
        }),
        "generic helper body should be added, not extracted from render: {:?}",
        result.changes
    );
    assert!(
        !result.changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionExtracted { name, .. } if name == "ready_label"
            )
        }),
        "generic helper body should not be extracted: {:?}",
        result.changes
    );
}

#[test]
fn repeated_diff_reuses_parse_cache_entries() {
    let cache = SemanticParseCache::default();
    cache.clear();

    let store = InMemoryStore::new();
    let from_tree = create_tree(
        &store,
        vec![("lib.rs", create_blob(&store, "fn before() -> i32 { 1 }\n"))],
    );
    let to_tree = create_tree(
        &store,
        vec![("lib.rs", create_blob(&store, "fn before() -> i32 { 2 }\n"))],
    );

    semantic_diff_with_cache(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
        &cache,
    )
    .unwrap_or_else(|err| panic!("first diff should succeed: {err}"));
    let after_first = cache.stats();

    semantic_diff_with_cache(
        &store,
        &from_tree,
        &to_tree,
        &SemanticDiffOptions::default(),
        &cache,
    )
    .unwrap_or_else(|err| panic!("second diff should succeed: {err}"));
    let after_second = cache.stats();

    assert!(
        after_first.misses > 0,
        "expected initial parse cache misses"
    );
    assert!(
        after_second.hits > after_first.hits,
        "expected parse cache hits to increase on repeat run: first={after_first:?} second={after_second:?}"
    );
}

#[test]
fn injected_cache_supports_worktree_diffs() {
    let cache = SemanticParseCache::default();
    let store = InMemoryStore::new();
    let from_tree = create_tree(
        &store,
        vec![("added.rs", create_blob(&store, "fn before() {}\n"))],
    );
    let worktree_root = std::env::temp_dir().join(format!(
        "heddle-semantic-cache-test-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&worktree_root).unwrap();
    std::fs::write(worktree_root.join("added.rs"), "fn after() {}\n").unwrap();
    let mut status = objects::worktree::WorktreeStatus::default();
    status.modified.push(std::path::PathBuf::from("added.rs"));

    let result = semantic_diff_worktree_with_cache(
        &store,
        &from_tree,
        &worktree_root,
        &status,
        &SemanticDiffOptions::default(),
        &cache,
    )
    .unwrap_or_else(|err| panic!("worktree diff should succeed: {err}"));

    assert!(!result.changes.is_empty());
    assert!(cache.stats().stores > 0);
}

#[derive(Clone)]
struct BenchRow {
    name: &'static str,
    fixture: BenchFixture,
    options: SemanticDiffOptions,
    expect_changed_file_fallback: bool,
    expect_parse_budget_fallback: bool,
}

#[derive(Clone)]
enum BenchFixture {
    Synthetic {
        repo_files: usize,
        changed_files: usize,
    },
    GitCommit {
        commit: &'static str,
        max_files: Option<usize>,
    },
}

#[test]
#[ignore = "semantic benchmark matrix; run explicitly when measuring large diff behavior"]
fn semantic_large_diff_benchmark_matrix() {
    let rows = [
        BenchRow {
            name: "10k repo / 100 changed / default budget",
            fixture: BenchFixture::Synthetic {
                repo_files: 10_000,
                changed_files: 100,
            },
            options: SemanticDiffOptions::default(),
            expect_changed_file_fallback: false,
            expect_parse_budget_fallback: false,
        },
        BenchRow {
            name: "10k repo / 10k changed / default fallback",
            fixture: BenchFixture::Synthetic {
                repo_files: 10_000,
                changed_files: 10_000,
            },
            options: SemanticDiffOptions::default(),
            expect_changed_file_fallback: true,
            expect_parse_budget_fallback: false,
        },
        BenchRow {
            name: "10k repo / 2048 changed / default ceiling",
            fixture: BenchFixture::Synthetic {
                repo_files: 10_000,
                changed_files: 2_048,
            },
            options: SemanticDiffOptions::default(),
            expect_changed_file_fallback: false,
            expect_parse_budget_fallback: true,
        },
        BenchRow {
            name: "10k repo / 2048 changed / raised parse budget",
            fixture: BenchFixture::Synthetic {
                repo_files: 10_000,
                changed_files: 2_048,
            },
            options: SemanticDiffOptions {
                budget: SemanticBudget {
                    max_changed_files: 10_000,
                    max_total_bytes: 256 * 1024 * 1024,
                    max_parsed_files: 4_096,
                    max_file_bytes: 1024 * 1024,
                },
                ..SemanticDiffOptions::default()
            },
            expect_changed_file_fallback: false,
            expect_parse_budget_fallback: false,
        },
        BenchRow {
            name: "10k repo / 5000 changed / raised budget",
            fixture: BenchFixture::Synthetic {
                repo_files: 10_000,
                changed_files: 5_000,
            },
            options: SemanticDiffOptions {
                budget: SemanticBudget {
                    max_changed_files: 10_000,
                    max_total_bytes: 512 * 1024 * 1024,
                    max_parsed_files: 10_000,
                    max_file_bytes: 1024 * 1024,
                },
                ..SemanticDiffOptions::default()
            },
            expect_changed_file_fallback: false,
            expect_parse_budget_fallback: false,
        },
        BenchRow {
            name: "10k repo / 300 changed / parse budget pressure",
            fixture: BenchFixture::Synthetic {
                repo_files: 10_000,
                changed_files: 300,
            },
            options: SemanticDiffOptions {
                budget: SemanticBudget {
                    max_changed_files: 10_000,
                    max_total_bytes: 64 * 1024 * 1024,
                    max_parsed_files: 512,
                    max_file_bytes: 1024 * 1024,
                },
                ..SemanticDiffOptions::default()
            },
            expect_changed_file_fallback: false,
            expect_parse_budget_fallback: true,
        },
        BenchRow {
            name: "real Heddle PR #38 / ingest-backed import provenance semantic hotspots",
            fixture: BenchFixture::GitCommit {
                commit: "ad04049b145304b46c327b5e2f3aeb9873555a1d",
                max_files: None,
            },
            options: SemanticDiffOptions {
                budget: SemanticBudget {
                    max_changed_files: 5_000,
                    max_total_bytes: 512 * 1024 * 1024,
                    max_parsed_files: 10_000,
                    max_file_bytes: 4 * 1024 * 1024,
                },
                ..SemanticDiffOptions::default()
            },
            expect_changed_file_fallback: false,
            expect_parse_budget_fallback: false,
        },
        BenchRow {
            name: "real Heddle PR #49 / v1 wire-format polish",
            fixture: BenchFixture::GitCommit {
                commit: "30334ad8c02a8162c746f928e516bdd45fb46aa6",
                max_files: None,
            },
            options: SemanticDiffOptions {
                budget: SemanticBudget {
                    max_changed_files: 5_000,
                    max_total_bytes: 512 * 1024 * 1024,
                    max_parsed_files: 10_000,
                    max_file_bytes: 4 * 1024 * 1024,
                },
                ..SemanticDiffOptions::default()
            },
            expect_changed_file_fallback: false,
            expect_parse_budget_fallback: false,
        },
    ];

    for row in rows {
        let store = InMemoryStore::new();
        let fixture = build_semantic_bench_fixture(&store, &row.fixture);
        let cache = SemanticParseCache::new(1024);
        let start = Instant::now();
        let result = semantic_diff_with_cache(
            &store,
            &fixture.from_tree,
            &fixture.to_tree,
            &row.options,
            &cache,
        )
        .unwrap_or_else(|err| panic!("benchmark row '{}' should run: {err}", row.name));
        let elapsed = start.elapsed();
        let stats = cache.stats();

        println!(
            "semantic bench row='{}' fixture={} repo_files={} changed_files={} elapsed={:?} changed_source_bytes={} fallback_summary={} cache={:?} changes={}",
            row.name,
            fixture.label,
            fixture.repo_files,
            fixture.changed_files,
            elapsed,
            fixture.changed_source_bytes,
            fallback_summary(&result.fallback_reasons),
            stats,
            result.changes.len()
        );

        assert_eq!(
            result.fallback_reasons.iter().any(|reason| matches!(
                reason,
                SemanticFallbackReason::ChangedFileBudgetExceeded { .. }
            )),
            row.expect_changed_file_fallback,
            "changed-file fallback mismatch for row '{}': {:?}",
            row.name,
            result.fallback_reasons
        );
        assert_eq!(
            result
                .fallback_reasons
                .iter()
                .any(|reason| matches!(reason, SemanticFallbackReason::ParseBudgetExceeded { .. })),
            row.expect_parse_budget_fallback,
            "parse-budget fallback mismatch for row '{}': {:?}",
            row.name,
            result.fallback_reasons
        );
    }
}

struct BenchFixtureData {
    label: String,
    from_tree: ContentHash,
    to_tree: ContentHash,
    repo_files: usize,
    changed_files: usize,
    changed_source_bytes: usize,
}

fn build_semantic_bench_fixture(store: &InMemoryStore, fixture: &BenchFixture) -> BenchFixtureData {
    match fixture {
        BenchFixture::Synthetic {
            repo_files,
            changed_files,
        } => {
            let (from_tree, to_tree, changed_source_bytes) =
                build_large_semantic_bench_trees(store, *repo_files, *changed_files);
            BenchFixtureData {
                label: "synthetic".to_string(),
                from_tree,
                to_tree,
                repo_files: *repo_files,
                changed_files: *changed_files,
                changed_source_bytes,
            }
        }
        BenchFixture::GitCommit { commit, max_files } => {
            build_git_commit_semantic_bench_trees(store, commit, *max_files)
        }
    }
}

fn build_large_semantic_bench_trees(
    store: &InMemoryStore,
    repo_files: usize,
    changed_files: usize,
) -> (ContentHash, ContentHash, usize) {
    assert!(changed_files <= repo_files);
    let unchanged_blob = create_blob(store, "fn stable() -> usize {\n    1\n}\n");
    let mut from_entries = Vec::with_capacity(repo_files);
    let mut to_entries = Vec::with_capacity(repo_files);
    let mut changed_source_bytes = 0usize;

    for index in 0..repo_files {
        let path = format!("file_{index:05}.rs");
        if index < changed_files {
            let old = semantic_bench_source(index, "old");
            let new = semantic_bench_source(index, "new");
            changed_source_bytes += old.len() + new.len();
            from_entries.push((path.clone(), create_blob(store, &old)));
            to_entries.push((path, create_blob(store, &new)));
        } else {
            from_entries.push((path.clone(), unchanged_blob));
            to_entries.push((path, unchanged_blob));
        }
    }

    (
        create_owned_tree(store, from_entries),
        create_owned_tree(store, to_entries),
        changed_source_bytes,
    )
}

fn semantic_bench_source(index: usize, variant: &str) -> String {
    format!(
        "pub fn compute_{index}(input: usize) -> usize {{\n    let base = input + {index};\n    let adjusted = base {} 2;\n    adjusted\n}}\n",
        if variant == "old" { "+" } else { "*" }
    )
}

fn build_git_commit_semantic_bench_trees(
    store: &InMemoryStore,
    commit: &str,
    max_files: Option<usize>,
) -> BenchFixtureData {
    let parent = git_stdout(["rev-parse", &format!("{commit}^")])
        .trim()
        .to_string();
    let file_list = git_stdout([
        "diff",
        "--name-only",
        "--diff-filter=ACDMR",
        &parent,
        commit,
    ]);
    let mut from_entries = Vec::new();
    let mut to_entries = Vec::new();
    let mut changed_source_bytes = 0usize;
    let mut changed_files = 0usize;

    for path in file_list
        .lines()
        .filter(|path| is_semantic_bench_path(path))
    {
        if max_files.is_some_and(|limit| changed_files >= limit) {
            break;
        }

        let old_content = git_show_text(&parent, path);
        let new_content = git_show_text(commit, path);
        if old_content.is_none() && new_content.is_none() {
            continue;
        }

        changed_source_bytes += old_content.as_ref().map_or(0, String::len);
        changed_source_bytes += new_content.as_ref().map_or(0, String::len);
        let bench_path = flatten_bench_path(path);
        if let Some(content) = old_content {
            from_entries.push((bench_path.clone(), create_blob(store, &content)));
        }
        if let Some(content) = new_content {
            to_entries.push((bench_path, create_blob(store, &content)));
        }
        changed_files += 1;
    }

    let repo_files = git_stdout(["ls-tree", "-r", "--name-only", commit])
        .lines()
        .filter(|path| is_semantic_bench_path(path))
        .count();

    BenchFixtureData {
        label: format!("git:{commit}"),
        from_tree: create_owned_tree(store, from_entries),
        to_tree: create_owned_tree(store, to_entries),
        repo_files,
        changed_files,
        changed_source_bytes,
    }
}

fn is_semantic_bench_path(path: &str) -> bool {
    matches!(
        path.rsplit('.').next(),
        Some(
            "rs" | "py"
                | "js"
                | "jsx"
                | "ts"
                | "tsx"
                | "go"
                | "c"
                | "cc"
                | "cpp"
                | "h"
                | "hpp"
                | "java"
        )
    )
}

fn flatten_bench_path(path: &str) -> String {
    path.replace('/', "__")
}

fn git_show_text(rev: &str, path: &str) -> Option<String> {
    let spec = format!("{rev}:{path}");
    let output = Command::new("git")
        .args(["show", &spec])
        .output()
        .unwrap_or_else(|err| panic!("failed to run git show {spec}: {err}"));
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

fn git_stdout<const N: usize>(args: [&str; N]) -> String {
    let output = Command::new("git")
        .args(args)
        .output()
        .unwrap_or_else(|err| panic!("failed to run git: {err}"));
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git output should be utf-8")
}

fn fallback_summary(reasons: &[SemanticFallbackReason]) -> String {
    if reasons.is_empty() {
        return "none".to_string();
    }

    let mut counts = BTreeMap::<&'static str, usize>::new();
    for reason in reasons {
        let key = match reason {
            SemanticFallbackReason::ChangedFileBudgetExceeded { .. } => "changed_file_budget",
            SemanticFallbackReason::TotalByteBudgetExceeded { .. } => "total_byte_budget",
            SemanticFallbackReason::FileTooLarge { .. } => "file_too_large",
            SemanticFallbackReason::ParseBudgetExceeded { .. } => "parse_budget",
            SemanticFallbackReason::UnsupportedLanguage { .. } => "unsupported_language",
            SemanticFallbackReason::ParseFailed { .. } => "parse_failed",
        };
        *counts.entry(key).or_default() += 1;
    }

    counts
        .into_iter()
        .map(|(key, count)| format!("{key}:{count}"))
        .collect::<Vec<_>>()
        .join(",")
}
