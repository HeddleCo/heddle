// SPDX-License-Identifier: Apache-2.0
use objects::object::{ChangeImportance, ModificationKind, SemanticChange};

use super::*;

fn nested_rust_modules(depth: usize, inner: &str) -> String {
    let mut source = String::new();
    for level in 0..depth {
        source.push_str(&format!("mod layer_{level} {{\n"));
    }
    source.push_str(inner);
    source.push('\n');
    for _ in 0..depth {
        source.push_str("}\n");
    }
    source
}

// ── Classification tests ──────────────────────────────────────────────

#[test]
fn test_classify_rust_formatting_only() {
    // Same tokens, different layout — rustfmt-style change.
    let old = "fn foo() {\nbar();\nbaz();\n}\n";
    let new = "fn foo() {\n    bar();\n    baz();\n}\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.rs"), old, new);
    assert_eq!(kind, ModificationKind::FormattingOnly);
    assert_eq!(imp, ChangeImportance::Noise);
}

#[test]
fn test_classify_javascript_formatting() {
    // Pure indentation change — no token difference.
    let old = "function greet(name) {\nreturn 'hello ' + name;\n}\n";
    let new = "function greet(name) {\n  return 'hello ' + name;\n}\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.js"), old, new);
    assert_eq!(kind, ModificationKind::FormattingOnly);
    assert_eq!(imp, ChangeImportance::Noise);
}

#[test]
fn test_classify_logic_change_rust() {
    let old = "fn compute() -> i32 {\n    42\n}\n";
    let new = "fn compute() -> i32 {\n    43\n}\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.rs"), old, new);
    assert_eq!(kind, ModificationKind::Logic);
    assert_eq!(imp, ChangeImportance::High);
}

#[test]
fn test_classify_comments_only_rust() {
    let old = "// Computes the answer.\nfn answer() -> i32 { 42 }\n";
    let new = "// Returns the final answer.\nfn answer() -> i32 { 42 }\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.rs"), old, new);
    assert_eq!(kind, ModificationKind::CommentsOnly);
    assert_eq!(imp, ChangeImportance::Low);
}

#[test]
fn test_classify_imports_only_rust() {
    let old = "use std::io;\n\nfn work() { do_stuff(); }\n";
    let new = "use std::io;\nuse std::fs;\n\nfn work() { do_stuff(); }\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.rs"), old, new);
    assert_eq!(kind, ModificationKind::ImportsOnly);
    assert_eq!(imp, ChangeImportance::Low);
}

#[test]
fn test_classify_mixed_change() {
    // Tokens are 90%+ similar but not identical — logic + formatting.
    let old = "fn foo() {\n    let x = 1;\n    let y = 2;\n    let z = 3;\n    let w = 4;\n    let v = 5;\n    let u = 6;\n    let t = 7;\n    let s = 8;\n    let r = 9;\n    x + y\n}\n";
    let new = "fn foo() {\n        let x = 1;\n        let y = 2;\n        let z = 3;\n        let w = 4;\n        let v = 5;\n        let u = 6;\n        let t = 7;\n        let s = 8;\n        let r = 9;\n        x + y + 1\n}\n";
    let (kind, _imp) = classify_modification(std::path::Path::new("test.rs"), old, new);
    // Should be Mixed or Logic — the exact classification depends on similarity thresholds,
    // but it should NOT be FormattingOnly since x + y changed to x + y + 1.
    assert!(
        kind == ModificationKind::Mixed || kind == ModificationKind::Logic,
        "Expected Mixed or Logic, got {:?}",
        kind
    );
}

#[test]
fn test_classify_empty_to_content() {
    let old = "";
    let new = "fn new_stuff() { 42 }\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.rs"), old, new);
    assert_eq!(kind, ModificationKind::Logic);
    assert_eq!(imp, ChangeImportance::High);
}

#[test]
fn test_classify_python_formatting() {
    let old = "def foo():\n    return 42\n";
    let new = "def foo():\n        return 42\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.py"), old, new);
    assert_eq!(kind, ModificationKind::FormattingOnly);
    assert_eq!(imp, ChangeImportance::Noise);
}

#[test]
fn test_classify_go_logic_change() {
    let old = "func main() {\n\tfmt.Println(\"hello\")\n}\n";
    let new = "func main() {\n\tfmt.Println(\"goodbye\")\n}\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.go"), old, new);
    assert_eq!(kind, ModificationKind::Logic);
    assert_eq!(imp, ChangeImportance::High);
}

#[test]
fn test_classify_unknown_language_formatting() {
    // Unknown language (.toml) — falls back to token-level.
    let old = "key = \"value\"\n";
    let new = "key  =  \"value\"\n";
    let (kind, imp) = classify_modification(std::path::Path::new("config.toml"), old, new);
    assert_eq!(kind, ModificationKind::FormattingOnly);
    assert_eq!(imp, ChangeImportance::Noise);
}

#[test]
fn test_classify_identical_content() {
    let content = "fn foo() { 42 }\n";
    let (kind, imp) = classify_modification(std::path::Path::new("test.rs"), content, content);
    assert_eq!(kind, ModificationKind::WhitespaceOnly);
    assert_eq!(imp, ChangeImportance::Noise);
}

// ── Function deletion / signature change tests ─────────────────────────

#[test]
fn test_detect_function_deletion() {
    let old = "fn kept() { 1 }\nfn removed() { 2 }\n";
    let new = "fn kept() { 1 }\n";
    let changes = detect_function_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    let has_deleted = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::FunctionDeleted { name, .. } if name == "removed"));
    assert!(
        has_deleted,
        "Expected FunctionDeleted for 'removed', got: {:?}",
        changes
    );
}

#[test]
fn test_detect_function_deletion_not_triggered_for_rename() {
    // When a function is renamed (high similarity), it should appear as FunctionRenamed, not deleted.
    // Needs enough content for reliable similarity matching (>0.7 threshold).
    let old = r#"fn old_name(input: &str) -> String {
    let trimmed = input.trim();
    let uppercased = trimmed.to_uppercase();
    let result = format!("Processed: {}", uppercased);
    println!("Processing: {}", input);
    println!("Result: {}", result);
    result
}"#;
    let new = r#"fn new_name(input: &str) -> String {
    let trimmed = input.trim();
    let uppercased = trimmed.to_uppercase();
    let result = format!("Processed: {}", uppercased);
    println!("Processing: {}", input);
    println!("Result: {}", result);
    result
}"#;
    let changes = detect_function_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    let has_rename = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::FunctionRenamed { .. }));
    let has_deleted = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::FunctionDeleted { .. }));
    assert!(has_rename, "Expected FunctionRenamed: {:?}", changes);
    assert!(
        !has_deleted,
        "Renamed function should not appear as deleted: {:?}",
        changes
    );
}

#[test]
fn test_detect_function_rename_with_body_update_not_delete_extract() {
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
    let changes = detect_function_changes(
        std::path::Path::new("repository_tests.rs"),
        std::path::Path::new("repository_tests.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    assert!(
        changes.iter().any(|change| {
            matches!(
                change,
                SemanticChange::FunctionRenamed { old_name, new_name, .. }
                    if old_name == "test_snapshot_uses_default_confidence_for_agent"
                        && new_name == "test_snapshot_without_confidence_records_none"
            )
        }),
        "expected rename for similar test update, got: {changes:?}"
    );
    assert!(
        !changes
            .iter()
            .any(|change| matches!(change, SemanticChange::FunctionDeleted { .. })),
        "rename should not also report deletion: {changes:?}"
    );
    assert!(
        !changes.iter().any(|change| matches!(
            change,
            SemanticChange::FunctionAdded { .. } | SemanticChange::FunctionExtracted { .. }
        )),
        "rename should not also report add/extract: {changes:?}"
    );
}

#[test]
fn test_detect_signature_change() {
    let old = "fn process(x: i32) -> i32 { x + 1 }\n";
    let new = "fn process(x: i32, y: i32) -> i32 { x + y }\n";
    let changes = detect_function_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    let has_sig_change = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::SignatureChanged { name, .. } if name == "process"));
    assert!(
        has_sig_change,
        "Expected SignatureChanged for 'process', got: {:?}",
        changes
    );
}

#[test]
fn test_detect_multiple_function_deletions() {
    let old = "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n";
    let new = "fn a() { 1 }\n";
    let changes = detect_function_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    let deleted_names: Vec<&str> = changes
        .iter()
        .filter_map(|c| match c {
            SemanticChange::FunctionDeleted { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        deleted_names.contains(&"b"),
        "Expected 'b' in deleted: {:?}",
        deleted_names
    );
    assert!(
        deleted_names.contains(&"c"),
        "Expected 'c' in deleted: {:?}",
        deleted_names
    );
}

#[test]
fn test_detect_signature_unchanged_body_changed() {
    // Same signature, different body — should NOT produce SignatureChanged.
    let old = "fn compute(x: i32) -> i32 { x + 1 }\n";
    let new = "fn compute(x: i32) -> i32 { x * 2 }\n";
    let changes = detect_function_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    let has_sig_change = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::SignatureChanged { .. }));
    assert!(
        !has_sig_change,
        "Body-only change should not be SignatureChanged: {:?}",
        changes
    );
    assert!(
        changes.iter().any(
            |c| matches!(c, SemanticChange::FunctionModified { name, .. } if name == "compute")
        ),
        "Body-only change should be FunctionModified: {:?}",
        changes
    );
}

// ── Import detection tests ─────────────────────────────────────────────

#[test]
fn test_detect_added_dependency() {
    let changes = detect_import_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        "",
        "use serde::Deserialize;\n",
    );
    let has_added = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::DependencyAdded { name, .. } if name == "serde"));
    assert!(
        has_added,
        "Expected DependencyAdded for serde: {:?}",
        changes
    );
}

#[test]
fn test_detect_removed_dependency() {
    let changes = detect_import_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        "use tokio::runtime;\n",
        "",
    );
    let has_removed = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::DependencyRemoved { name } if name == "tokio"));
    assert!(
        has_removed,
        "Expected DependencyRemoved for tokio: {:?}",
        changes
    );
}

#[test]
fn test_detect_grouped_use_edit_does_not_report_dependency_churn() {
    let changes = detect_import_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        "use objects::{\n    object::{Blob, Tree},\n    store::ObjectStore,\n};\n",
        "use objects::{\n    object::{Blob, SemanticChange, Tree},\n    store::ObjectStore,\n};\n",
    );
    assert!(
        changes.is_empty(),
        "Editing members of an existing grouped use should not look like dependency add/remove: {changes:?}"
    );
}

#[test]
fn test_stdlib_imports_ignored() {
    // Switching between std imports should produce no dependency changes.
    let changes = detect_import_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        "use std::io::Read;\n",
        "use std::io::Write;\nuse core::fmt;\n",
    );
    assert!(
        changes.is_empty(),
        "Stdlib changes should be ignored: {:?}",
        changes
    );
}

// ── Similarity edge cases ──────────────────────────────────────────────

#[test]
fn test_similarity_empty_strings() {
    assert_eq!(compute_similarity("", "", SimilarityMethod::Lines), 1.0);
    assert_eq!(compute_similarity("", "", SimilarityMethod::Tokens), 1.0);
}

#[test]
fn test_similarity_one_empty() {
    assert_eq!(
        compute_similarity("content", "", SimilarityMethod::Lines),
        0.0
    );
    assert_eq!(
        compute_similarity("", "content", SimilarityMethod::Tokens),
        0.0
    );
}

#[test]
fn test_similarity_whitespace_only_difference() {
    let a = "foo bar baz";
    let b = "foo  bar  baz";
    let token_sim = compute_similarity(a, b, SimilarityMethod::Tokens);
    assert_eq!(
        token_sim, 1.0,
        "Whitespace-only diff should have token similarity 1.0"
    );
}

// ── Original tests ────────────────────────────────────────────────────

#[test]
fn test_compute_similarity_lines() {
    let a = "line1\nline2\nline3";
    let b = "line1\nline2\nline3";
    assert_eq!(compute_similarity(a, b, SimilarityMethod::Lines), 1.0);

    let c = "line1\nline2\nline4";
    let sim = compute_similarity(a, c, SimilarityMethod::Lines);
    assert!(sim >= 0.5);

    let d = "completely different";
    let sim = compute_similarity(a, d, SimilarityMethod::Lines);
    assert!(sim < 0.5);
}

#[test]
fn test_detect_function_changes_rename() {
    let old = r#"fn old_function_name() {
    println!("Hello");
    let x = 42;
    println!("{}", x);
}"#;
    let new = r#"fn new_function_name() {
    println!("Hello");
    let x = 42;
    println!("{}", x);
}"#;

    let changes = detect_function_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );

    assert!(!changes.is_empty());
    let has_rename_or_extract = changes.iter().any(|c| {
        matches!(c, SemanticChange::FunctionRenamed { .. })
            || matches!(c, SemanticChange::FunctionAdded { .. })
            || matches!(c, SemanticChange::FunctionExtracted { .. })
    });
    assert!(has_rename_or_extract);
}

#[test]
fn test_detect_function_changes_prefers_stable_best_rename_candidate() {
    let old = concat!(
        "fn alpha() {\n",
        "    let value = 1;\n",
        "    let doubled = value * 2;\n",
        "    let message = format!(\"value={} doubled={}\", value, doubled);\n",
        "    println!(\"{}\", message);\n",
        "    println!(\"{}\", doubled);\n",
        "}\n\n",
        "fn beta() {\n",
        "    let value = 1;\n",
        "    let doubled = value * 2;\n",
        "    let message = format!(\"value={} doubled={}\", value, doubled);\n",
        "    println!(\"{}\", message);\n",
        "    println!(\"{}\", doubled);\n",
        "}\n",
    );
    let new = concat!(
        "fn gamma() {\n",
        "    let value = 1;\n",
        "    let doubled = value * 2;\n",
        "    let message = format!(\"value={} doubled={}\", value, doubled);\n",
        "    println!(\"{}\", message);\n",
        "    println!(\"{}\", doubled);\n",
        "}\n",
    );

    let changes = detect_function_changes(
        std::path::Path::new("test.rs"),
        std::path::Path::new("test.rs"),
        old,
        new,
        SimilarityMethod::Lines,
    );

    assert!(changes.iter().any(|change| matches!(
        change,
        SemanticChange::FunctionRenamed { old_name, new_name, .. }
        if old_name == "alpha" && new_name == "gamma"
    )));
    assert!(changes.iter().any(|change| matches!(
        change,
        SemanticChange::FunctionDeleted { name, .. } if name == "beta"
    )));
}

#[test]
fn test_detect_file_renames() {
    let deleted = vec![(
        std::path::PathBuf::from("old.rs"),
        "fn main() {}".to_string(),
    )];
    let added = vec![(
        std::path::PathBuf::from("new.rs"),
        "fn main() {}".to_string(),
    )];

    let renames = detect_file_renames(&deleted, &added, 0.8, SimilarityMethod::Lines);
    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].0, std::path::PathBuf::from("old.rs"));
    assert_eq!(renames[0].1, std::path::PathBuf::from("new.rs"));
}

#[test]
fn test_compute_similarity_ast_fallback() {
    let a = "fn add(a: i32, b: i32) -> i32 { a + b }";
    let b = "fn sum(x: i32, y: i32) -> i32 { x + y }";
    let sim = super::analysis_similarity::compute_similarity_with_language(
        a,
        b,
        SimilarityMethod::Ast,
        crate::parser::Language::Rust,
    );
    assert!(sim > 0.6);
}

#[test]
fn test_classify_comments_only_handles_deep_ast_without_recursion() {
    let old = nested_rust_modules(512, "// old\nfn deeply_nested() { println!(\"hi\"); }");
    let new = nested_rust_modules(512, "// new\nfn deeply_nested() { println!(\"hi\"); }");

    let (kind, importance) = classify_modification(std::path::Path::new("test.rs"), &old, &new);
    assert_eq!(kind, ModificationKind::CommentsOnly);
    assert_eq!(importance, ChangeImportance::Low);
}

#[test]
fn test_ast_similarity_handles_deep_ast_without_recursion() {
    let source = nested_rust_modules(512, "fn deeply_nested() { println!(\"hi\"); }");

    let similarity = super::analysis_similarity::compute_similarity_with_language(
        &source,
        &source,
        SimilarityMethod::Ast,
        crate::parser::Language::Rust,
    );

    assert_eq!(similarity, 1.0);
}

#[test]
fn test_extract_dependency_from_import() {
    assert_eq!(
        super::analysis_imports::detect_import_changes(
            std::path::Path::new("test.rs"),
            std::path::Path::new("test.rs"),
            "use serde::Deserialize;",
            "use std::collections::HashMap;"
        )
        .len(),
        1
    );
}

// ── Duplicate-name redeclaration tests (BTreeMap-collapse hazard) ─────
//
// Mirrors the heddle#114 r5 P1 #2 fix (commit 2198b00) — keying per-side
// item maps by bare name collapses same-name redeclarations into the
// last occurrence, silently dropping earlier definitions. Both JS and
// Python permit module-level redeclaration of the same function name.

#[test]
fn test_detect_duplicate_javascript_function_redeclarations_both_surface() {
    // Two top-level `function foo()` declarations, both modified between
    // old and new. A BTreeMap<String, FunctionDef> keyed on `f.name`
    // would keep only the second `foo` on each side, so only one
    // FunctionModified event would fire instead of two.
    let old = "function foo() { return 1; }\nfunction foo() { return 2; }\n";
    let new = "function foo() { return 10; }\nfunction foo() { return 20; }\n";
    let changes = detect_function_changes(
        std::path::Path::new("test.js"),
        std::path::Path::new("test.js"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    let modified_count = changes
        .iter()
        .filter(|c| matches!(c, SemanticChange::FunctionModified { name, .. } if name == "foo"))
        .count();
    assert_eq!(
        modified_count, 2,
        "Both `foo` redeclarations should surface as FunctionModified; got: {:?}",
        changes
    );
}

#[test]
fn test_detect_duplicate_python_function_redefinitions_both_surface() {
    // Python permits top-level `def foo()` redefinition; only the last
    // wins at runtime, but both occurrences exist in the AST and should
    // both surface as changes when their bodies move between versions.
    let old = "def foo():\n    return 1\n\ndef foo():\n    return 2\n";
    let new = "def foo():\n    return 10\n\ndef foo():\n    return 20\n";
    let changes = detect_function_changes(
        std::path::Path::new("test.py"),
        std::path::Path::new("test.py"),
        old,
        new,
        SimilarityMethod::Lines,
    );
    let modified_count = changes
        .iter()
        .filter(|c| matches!(c, SemanticChange::FunctionModified { name, .. } if name == "foo"))
        .count();
    assert_eq!(
        modified_count, 2,
        "Both `foo` redefinitions should surface as FunctionModified; got: {:?}",
        changes
    );
}

// ── Insert-before-existing tests (positional-pairing breakage) ────────
//
// Pins the fix for the Codex finding on heddle#125 r2 (cid 3259311747):
// per-side `(name, occurrence)` keying paired old `foo[0]` with new
// `foo[0]` regardless of body content, so a fresh same-name definition
// inserted *before* existing ones produced a bogus `FunctionModified`
// (with the wrong content delta) plus a misclassified add/delete pair.
// Content-similarity matching across the same-name bucket aligns the
// surviving body with itself instead of with the inserted one.

#[test]
fn test_detect_insert_before_same_name_pairs_by_content_not_position() {
    // Codex's example, expanded with distinct bodies so the mispairing
    // is observable. Old has `bar(A)` and `foo(B)`; new has `bar(B)`
    // (renamed-from-foo) followed by `bar(A)` (the original, pushed
    // down one slot). Correct interpretation: foo → bar(B) rename,
    // bar(A) survives unchanged.
    let old = concat!(
        "function bar() {\n",
        "    let value = 1;\n",
        "    let doubled = value * 2;\n",
        "    console.log('marker_alpha', value, doubled);\n",
        "    return doubled;\n",
        "}\n",
        "function foo() {\n",
        "    const greeting = 'hello world';\n",
        "    const repeated = greeting.repeat(3);\n",
        "    console.log('marker_beta', greeting, repeated);\n",
        "    return repeated;\n",
        "}\n",
    );
    let new = concat!(
        "function bar() {\n",
        "    const greeting = 'hello world';\n",
        "    const repeated = greeting.repeat(3);\n",
        "    console.log('marker_beta', greeting, repeated);\n",
        "    return repeated;\n",
        "}\n",
        "function bar() {\n",
        "    let value = 1;\n",
        "    let doubled = value * 2;\n",
        "    console.log('marker_alpha', value, doubled);\n",
        "    return doubled;\n",
        "}\n",
    );
    let changes = detect_function_changes(
        std::path::Path::new("test.js"),
        std::path::Path::new("test.js"),
        old,
        new,
        SimilarityMethod::Lines,
    );

    let has_foo_to_bar_rename = changes.iter().any(|c| {
        matches!(
            c,
            SemanticChange::FunctionRenamed { old_name, new_name, .. }
                if old_name == "foo" && new_name == "bar"
        )
    });
    assert!(
        has_foo_to_bar_rename,
        "foo's body re-appearing under `bar` should be a FunctionRenamed{{foo→bar}}; got: {:?}",
        changes
    );

    let has_modified = changes
        .iter()
        .any(|c| matches!(c, SemanticChange::FunctionModified { .. }));
    assert!(
        !has_modified,
        "Inserting a same-name definition before an existing one must not synthesize a \
         FunctionModified — the original body survived intact: {:?}",
        changes
    );
}

#[test]
fn test_detect_same_name_insert_at_front_keeps_existing_body() {
    // Smaller version of the same hazard: old has a single `foo(A)`;
    // new prepends a fresh `foo(B)`. Correct: `foo(A)` is preserved
    // (matched against itself by content), `foo(B)` is added.
    // Positional pairing instead matches old `foo[0]=A` with new
    // `foo[0]=B`, surfacing a spurious FunctionModified and reporting
    // the surviving A as a new add.
    let old = concat!(
        "function foo() {\n",
        "    let value = 1;\n",
        "    let doubled = value * 2;\n",
        "    console.log('marker_alpha', value, doubled);\n",
        "    return doubled;\n",
        "}\n",
    );
    let new = concat!(
        "function foo() {\n",
        "    const greeting = 'hello world';\n",
        "    const repeated = greeting.repeat(3);\n",
        "    console.log('marker_beta', greeting, repeated);\n",
        "    return repeated;\n",
        "}\n",
        "function foo() {\n",
        "    let value = 1;\n",
        "    let doubled = value * 2;\n",
        "    console.log('marker_alpha', value, doubled);\n",
        "    return doubled;\n",
        "}\n",
    );
    let changes = detect_function_changes(
        std::path::Path::new("test.js"),
        std::path::Path::new("test.js"),
        old,
        new,
        SimilarityMethod::Lines,
    );

    let modified_count = changes
        .iter()
        .filter(|c| matches!(c, SemanticChange::FunctionModified { name, .. } if name == "foo"))
        .count();
    assert_eq!(
        modified_count, 0,
        "Surviving foo body must not be reported as FunctionModified: {:?}",
        changes
    );

    let added_count = changes
        .iter()
        .filter(|c| matches!(c, SemanticChange::FunctionAdded { name, .. } if name == "foo"))
        .count();
    assert_eq!(
        added_count, 1,
        "The inserted foo should surface as a single FunctionAdded: {:?}",
        changes
    );
}

#[test]
fn test_is_stdlib_dependency() {
    assert!(
        super::analysis_imports::detect_import_changes(
            std::path::Path::new("test.rs"),
            std::path::Path::new("test.rs"),
            "use std::collections::HashMap;",
            "use std::io::Read;"
        )
        .is_empty()
    );
}