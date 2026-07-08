// SPDX-License-Identifier: Apache-2.0
//! Change classification engine — determines whether a file modification is
//! logic, formatting, imports-only, comments-only, or mixed.

use std::path::Path;

use objects::object::{ChangeImportance, ModificationKind};

use super::analysis_similarity::{compute_similarity, SimilarityMethod};
use crate::parser::{Language, ParsedFile};

/// Classification result: kind, importance, and confidence.
pub type ClassificationResult = (ModificationKind, ChangeImportance, f64);

/// Classify what kind of modification happened to a file and its review importance.
///
/// This is the core engine behind "147 files changed → 11 things worth reviewing":
/// it separates noise (formatting, imports, comments) from signal (logic changes).
///
/// Returns (kind, importance, confidence) where confidence is 0.0–1.0.
/// AST-backed classification gets high confidence (0.9+), token-fallback gets medium (0.6–0.7).
pub fn classify_modification(
    path: &Path,
    old_content: &str,
    new_content: &str,
) -> (ModificationKind, ChangeImportance) {
    let (kind, importance, _confidence) =
        classify_modification_with_confidence(path, old_content, new_content);
    (kind, importance)
}

/// Like `classify_modification` but also returns a confidence score.
pub fn classify_modification_with_confidence(
    path: &Path,
    old_content: &str,
    new_content: &str,
) -> ClassificationResult {
    let token_sim = classify_common_prefix(old_content, new_content);
    if let Some(result) = token_sim.result {
        return result;
    }

    let language = Language::from_path(path);

    // Try AST-based classification. Falls back to token-level if parsing fails.
    let old_parsed = ParsedFile::parse(old_content, language);
    let new_parsed = ParsedFile::parse(new_content, language);

    classify_parse_result(
        old_content,
        new_content,
        old_parsed.as_ref(),
        new_parsed.as_ref(),
        token_sim.value,
    )
}

pub(crate) fn classify_modification_with_parsed(
    old_content: &str,
    new_content: &str,
    old_ast: &ParsedFile,
    new_ast: &ParsedFile,
) -> ClassificationResult {
    let token_sim = classify_common_prefix(old_content, new_content);
    if let Some(result) = token_sim.result {
        return result;
    }

    classify_with_ast(old_content, new_content, old_ast, new_ast)
}

struct TokenSimilarityCheck {
    value: f64,
    result: Option<ClassificationResult>,
}

fn classify_common_prefix(old_content: &str, new_content: &str) -> TokenSimilarityCheck {
    // Identical content should not reach here, but handle it gracefully.
    if old_content == new_content {
        return TokenSimilarityCheck {
            value: 1.0,
            result: Some((
                ModificationKind::WhitespaceOnly,
                ChangeImportance::Noise,
                1.0,
            )),
        };
    }

    // --- Check 1: Token-identical means formatting/whitespace only ---
    let token_sim = compute_similarity(old_content, new_content, SimilarityMethod::Tokens);
    if token_sim >= 1.0 {
        // Tokens are identical but raw text differs → pure formatting/whitespace.
        // High confidence: token identity is a strong signal.
        return TokenSimilarityCheck {
            value: token_sim,
            result: Some((
                ModificationKind::FormattingOnly,
                ChangeImportance::Noise,
                0.95,
            )),
        };
    }

    TokenSimilarityCheck {
        value: token_sim,
        result: None,
    }
}

fn classify_parse_result(
    old_content: &str,
    new_content: &str,
    old_parsed: Option<&ParsedFile>,
    new_parsed: Option<&ParsedFile>,
    token_sim: f64,
) -> ClassificationResult {
    match (old_parsed, new_parsed) {
        (Some(old_ast), Some(new_ast)) => {
            classify_with_ast(old_content, new_content, old_ast, new_ast)
        }
        _ => {
            // Parse failed — fall back to token-level heuristics (lower confidence).
            classify_without_ast(old_content, new_content, token_sim)
        }
    }
}

/// AST-backed classification — the most accurate path.
fn classify_with_ast(
    old_content: &str,
    new_content: &str,
    old_ast: &ParsedFile,
    new_ast: &ParsedFile,
) -> ClassificationResult {
    let old_funcs = old_ast.extract_functions();
    let new_funcs = new_ast.extract_functions();
    let old_imports = old_ast.extract_imports();
    let new_imports = new_ast.extract_imports();

    let funcs_identical = are_functions_identical(&old_funcs, &new_funcs);
    let imports_identical = old_imports.len() == new_imports.len()
        && old_imports
            .iter()
            .zip(new_imports.iter())
            .all(|(a, b)| a.raw == b.raw);

    // Check comments-only: strip comments from both and compare.
    let old_stripped = strip_comments(old_ast);
    let new_stripped = strip_comments(new_ast);
    let non_comment_identical = old_stripped == new_stripped;

    if non_comment_identical {
        return (ModificationKind::CommentsOnly, ChangeImportance::Low, 0.92);
    }

    if funcs_identical && !imports_identical {
        // Functions haven't changed, only imports differ.
        // Double-check that non-import, non-function code is also identical.
        let old_body = strip_imports_and_functions(old_ast);
        let new_body = strip_imports_and_functions(new_ast);
        if old_body == new_body {
            return (ModificationKind::ImportsOnly, ChangeImportance::Low, 0.93);
        }
    }

    // Check if token-equivalent (formatting only) but AST was parseable.
    let token_sim = compute_similarity(old_content, new_content, SimilarityMethod::Tokens);
    if token_sim >= 1.0 {
        return (
            ModificationKind::FormattingOnly,
            ChangeImportance::Noise,
            0.97,
        );
    }

    // If functions changed but formatting also changed, it's mixed.
    // Heuristic: compute line similarity to detect formatting noise alongside logic.
    let line_sim = compute_similarity(old_content, new_content, SimilarityMethod::Lines);
    if token_sim > 0.9 && line_sim < 0.7 {
        // High token overlap but low line overlap → mostly formatting with some logic.
        return (ModificationKind::Mixed, ChangeImportance::Medium, 0.75);
    }

    // Default: real logic change. AST-backed so reasonably confident.
    (ModificationKind::Logic, ChangeImportance::High, 0.85)
}

/// Token-level fallback when tree-sitter parsing fails (lower confidence).
fn classify_without_ast(
    old_content: &str,
    new_content: &str,
    token_sim: f64,
) -> ClassificationResult {
    if token_sim >= 1.0 {
        return (
            ModificationKind::FormattingOnly,
            ChangeImportance::Noise,
            0.9,
        );
    }

    let line_sim = compute_similarity(old_content, new_content, SimilarityMethod::Lines);

    // High token similarity + low line similarity → mostly formatting.
    if token_sim > 0.95 && line_sim < 0.8 {
        return (
            ModificationKind::FormattingOnly,
            ChangeImportance::Noise,
            0.7,
        );
    }

    if token_sim > 0.9 {
        return (ModificationKind::Mixed, ChangeImportance::Medium, 0.6);
    }

    // Token-level fallback — lower confidence since we can't parse the AST.
    (ModificationKind::Logic, ChangeImportance::High, 0.5)
}

/// Compare function lists for identity (same names, same content).
fn are_functions_identical(
    old_funcs: &[crate::parser::FunctionDef],
    new_funcs: &[crate::parser::FunctionDef],
) -> bool {
    if old_funcs.len() != new_funcs.len() {
        return false;
    }
    // Sort by name for stable comparison.
    let mut old_sorted: Vec<_> = old_funcs.iter().collect();
    let mut new_sorted: Vec<_> = new_funcs.iter().collect();
    old_sorted.sort_by_key(|f| &f.name);
    new_sorted.sort_by_key(|f| &f.name);

    old_sorted
        .iter()
        .zip(new_sorted.iter())
        .all(|(a, b)| a.name == b.name && a.content == b.content)
}

/// Walk the AST and collect text of all non-comment nodes.
fn strip_comments(parsed: &ParsedFile) -> String {
    let mut result = String::new();
    collect_non_comment_text(parsed.root_node(), &parsed.source, &mut result);
    result
}

fn collect_non_comment_text(node: tree_sitter::Node<'_>, source: &str, out: &mut String) {
    let mut stack = vec![node];

    while let Some(current) = stack.pop() {
        if is_comment_node(current.kind()) {
            continue;
        }

        if current.child_count() == 0 {
            out.push_str(&source[current.byte_range()]);
            out.push(' ');
            continue;
        }

        let child_count = current.child_count();
        for index in (0..child_count).rev() {
            if let Some(child) = current.child(index as u32) {
                stack.push(child);
            }
        }
    }
}

fn is_comment_node(kind: &str) -> bool {
    matches!(
        kind,
        "comment" | "line_comment" | "block_comment" | "doc_comment" | "string_comment"
    )
}

/// Strip imports and function bodies, return remaining "scaffold" text.
fn strip_imports_and_functions(parsed: &ParsedFile) -> String {
    let mut result = String::new();
    let root = parsed.root_node();
    for i in 0..root.child_count() {
        if let Some(child) = root.child(i as u32) {
            let kind = child.kind();
            // Skip imports.
            if matches!(
                kind,
                "use_declaration"
                    | "extern_crate_declaration"
                    | "import_statement"
                    | "import_from_statement"
                    | "import_declaration"
            ) {
                continue;
            }
            // Skip function definitions.
            if ParsedFile::is_function_kind(kind, parsed.language) {
                continue;
            }
            result.push_str(&parsed.source[child.byte_range()]);
            result.push('\n');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_whitespace_only() {
        let old = "fn foo() {\n    bar();\n}\n";
        let new = "fn foo() {\n        bar();\n}\n";
        let (kind, importance) = classify_modification(Path::new("test.rs"), old, new);
        assert_eq!(kind, ModificationKind::FormattingOnly);
        assert_eq!(importance, ChangeImportance::Noise);
    }

    #[test]
    fn test_logic_change() {
        let old = "fn foo() -> i32 {\n    42\n}\n";
        let new = "fn foo() -> i32 {\n    43\n}\n";
        let (kind, importance) = classify_modification(Path::new("test.rs"), old, new);
        assert_eq!(kind, ModificationKind::Logic);
        assert_eq!(importance, ChangeImportance::High);
    }

    #[test]
    fn test_comments_only() {
        let old = "// old comment\nfn foo() {\n    bar();\n}\n";
        let new = "// new comment\nfn foo() {\n    bar();\n}\n";
        let (kind, importance) = classify_modification(Path::new("test.rs"), old, new);
        assert_eq!(kind, ModificationKind::CommentsOnly);
        assert_eq!(importance, ChangeImportance::Low);
    }

    #[test]
    fn test_imports_only() {
        let old = "use std::io;\n\nfn foo() {\n    bar();\n}\n";
        let new = "use std::io;\nuse std::fs;\n\nfn foo() {\n    bar();\n}\n";
        let (kind, importance) = classify_modification(Path::new("test.rs"), old, new);
        assert_eq!(kind, ModificationKind::ImportsOnly);
        assert_eq!(importance, ChangeImportance::Low);
    }

    #[test]
    fn test_parse_error_fallback() {
        // Unknown language — falls back to token-level classification.
        let old = "some content here\n";
        let new = "some content here\nwith additions\n";
        let (kind, importance) = classify_modification(Path::new("test.xyz"), old, new);
        // Should classify as Logic since tokens differ and we can't parse.
        assert_eq!(kind, ModificationKind::Logic);
        assert_eq!(importance, ChangeImportance::High);
    }

    #[test]
    fn test_formatting_only_unknown_lang() {
        // Token-identical but line-different on unknown language.
        let old = "foo bar baz\n";
        let new = "foo  bar  baz\n";
        let (kind, importance) = classify_modification(Path::new("test.xyz"), old, new);
        assert_eq!(kind, ModificationKind::FormattingOnly);
        assert_eq!(importance, ChangeImportance::Noise);
    }

    #[test]
    fn test_classify_with_parsed_matches_direct_classifier() {
        let old = "use std::io;\n\nfn compute() -> i32 {\n    1\n}\n";
        let new = "use std::io;\nuse std::fs;\n\nfn compute() -> i32 {\n    1\n}\n";
        let old_ast = ParsedFile::parse(old, Language::Rust).expect("old Rust should parse");
        let new_ast = ParsedFile::parse(new, Language::Rust).expect("new Rust should parse");

        let direct = classify_modification_with_confidence(Path::new("test.rs"), old, new);
        let cached = classify_modification_with_parsed(old, new, &old_ast, &new_ast);

        assert_eq!(cached, direct);
    }
}
