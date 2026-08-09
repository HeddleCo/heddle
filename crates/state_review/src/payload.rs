// SPDX-License-Identifier: Apache-2.0
//! Review reading-order builder.
//!
//! Pure function that classifies the changed symbols on a state into
//! the three reading-order partitions:
//!
//! - **Structural** — symbols whose *shape* changed (types, traits,
//!   classes, function signatures, type aliases, enum definitions,
//!   const decls, modules). Read these first; they constrain everything
//!   downstream.
//! - **Consequence** — function bodies that implement behavior. Read
//!   second; meaning depends on the structural pieces.
//! - **Tests and docs** — anything under a test path or with a
//!   doc/spec extension. Read last; treat as confirmation rather than
//!   load-bearing logic.
//!
//! This module owns the partitioning policy. Local review and hosted
//! boundaries consume the same domain result rather than re-classifying it.

use std::path::Path;

/// One symbol's location on disk. Pure-data — no rendering decisions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathSymbol {
    pub file: String,
    pub symbol: String,
    /// Symbol kind reported by the parser. Used to decide
    /// structural-vs-consequence; if unknown the heuristic falls
    /// through to consequence.
    pub kind: SymbolKind,
}

/// Durable symbol taxonomy shared with semantic indexes.
pub use objects::object::SymbolKindTag as SymbolKind;

fn is_structural(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Type
            | SymbolKind::Trait
            | SymbolKind::Class
            | SymbolKind::Interface
            | SymbolKind::TypeAlias
            | SymbolKind::Enum
            | SymbolKind::Const
            | SymbolKind::Module
    )
}

/// Reading-order classification for a set of changed symbols.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReadingOrderPartition {
    pub structural: Vec<PathSymbol>,
    pub consequence: Vec<PathSymbol>,
    pub tests_and_docs: Vec<PathSymbol>,
}

/// Build the partition. Symbols arrive in source order; the function
/// preserves order within each partition so the client gets a stable
/// rendering.
pub fn build_review_payload_partition(symbols: Vec<PathSymbol>) -> ReadingOrderPartition {
    let mut structural = Vec::new();
    let mut consequence = Vec::new();
    let mut tests_and_docs = Vec::new();

    for sym in symbols {
        if is_test_or_docs_path(&sym.file) {
            tests_and_docs.push(sym);
        } else if is_structural(sym.kind) {
            structural.push(sym);
        } else {
            consequence.push(sym);
        }
    }

    ReadingOrderPartition {
        structural,
        consequence,
        tests_and_docs,
    }
}

/// Match paths the plan says always belong in the tests_and_docs tier:
/// `(test|spec|__tests__|_test\.go)` anywhere in the path, plus
/// `.md|.rst|.txt` extensions. Documentation suffixes outweigh
/// symbol-kind: a constant defined in `README.md` is still docs.
fn is_test_or_docs_path(path: &str) -> bool {
    let p = Path::new(path);
    if let Some(ext) = p.extension().and_then(|e| e.to_str())
        && matches!(ext.to_ascii_lowercase().as_str(), "md" | "rst" | "txt")
    {
        return true;
    }
    let lowered = path.to_ascii_lowercase();
    if lowered.ends_with("_test.go") {
        return true;
    }
    // Component-level matches — `_test.py` and `*.spec.ts` typically
    // mark tests, but a containing-directory match catches the
    // codebase-specific `__tests__/` and `tests/` conventions too.
    let segments: Vec<&str> = lowered.split('/').collect();
    for seg in &segments {
        if *seg == "tests"
            || *seg == "test"
            || *seg == "__tests__"
            || seg.ends_with(".test.ts")
            || seg.ends_with(".test.tsx")
            || seg.ends_with(".test.js")
            || seg.ends_with(".test.jsx")
            || seg.ends_with(".spec.ts")
            || seg.ends_with(".spec.tsx")
            || seg.ends_with(".spec.js")
            || seg.ends_with(".spec.jsx")
        {
            return true;
        }
    }
    // Trailing `_test` filename (Go convention without `.go`, plus
    // some Python projects).
    if let Some(file_stem) = p.file_stem().and_then(|s| s.to_str())
        && (file_stem.ends_with("_test") || file_stem.starts_with("test_"))
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ps(file: &str, symbol: &str, kind: SymbolKind) -> PathSymbol {
        PathSymbol {
            file: file.to_string(),
            symbol: symbol.to_string(),
            kind,
        }
    }

    #[test]
    fn structural_kinds_land_in_structural_partition() {
        let symbols = vec![
            ps("src/lib.rs", "Foo", SymbolKind::Type),
            ps("src/lib.rs", "Bar", SymbolKind::Trait),
            ps("src/lib.rs", "Color", SymbolKind::Enum),
        ];
        let part = build_review_payload_partition(symbols);
        assert_eq!(part.structural.len(), 3);
        assert!(part.consequence.is_empty());
        assert!(part.tests_and_docs.is_empty());
    }

    #[test]
    fn function_body_lands_in_consequence_partition() {
        let symbols = vec![ps("src/lib.rs", "compute", SymbolKind::Function)];
        let part = build_review_payload_partition(symbols);
        assert_eq!(part.consequence.len(), 1);
        assert!(part.structural.is_empty());
    }

    #[test]
    fn test_paths_outweigh_symbol_kind() {
        // A `Type` definition in a test file is still tests_and_docs.
        let symbols = vec![ps("crates/foo/tests/it.rs", "Sample", SymbolKind::Type)];
        let part = build_review_payload_partition(symbols);
        assert_eq!(part.tests_and_docs.len(), 1);
        assert!(part.structural.is_empty());
    }

    #[test]
    fn go_test_file_naming_convention_recognized() {
        let symbols = vec![ps("pkg/foo_test.go", "TestFoo", SymbolKind::Function)];
        let part = build_review_payload_partition(symbols);
        assert_eq!(part.tests_and_docs.len(), 1);
    }

    #[test]
    fn documentation_extensions_are_tests_and_docs() {
        let symbols = vec![
            ps("README.md", "TopLevel", SymbolKind::Other),
            ps("docs/intro.rst", "Intro", SymbolKind::Other),
            ps("notes.txt", "Note", SymbolKind::Other),
        ];
        let part = build_review_payload_partition(symbols);
        assert_eq!(part.tests_and_docs.len(), 3);
    }

    #[test]
    fn web_test_files_recognized_by_suffix() {
        let symbols = vec![
            ps(
                "web/src/foo.test.ts",
                "describe_block",
                SymbolKind::Function,
            ),
            ps("web/src/bar.spec.tsx", "render_test", SymbolKind::Function),
        ];
        let part = build_review_payload_partition(symbols);
        assert_eq!(part.tests_and_docs.len(), 2);
    }

    #[test]
    fn unknown_kind_falls_through_to_consequence() {
        let symbols = vec![ps("src/lib.rs", "mystery", SymbolKind::Other)];
        let part = build_review_payload_partition(symbols);
        assert_eq!(part.consequence.len(), 1);
    }

    #[test]
    fn order_is_preserved_within_partition() {
        let symbols = vec![
            ps("src/a.rs", "A", SymbolKind::Type),
            ps("src/b.rs", "B", SymbolKind::Type),
            ps("src/c.rs", "C", SymbolKind::Type),
        ];
        let part = build_review_payload_partition(symbols);
        assert_eq!(
            part.structural
                .iter()
                .map(|s| s.symbol.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C"]
        );
    }
}
