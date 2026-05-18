// SPDX-License-Identifier: Apache-2.0
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

#[test]
fn test_language_from_path() {
    assert_eq!(
        Language::from_path(std::path::Path::new("foo.rs")),
        Language::Rust
    );
    assert_eq!(
        Language::from_path(std::path::Path::new("foo.py")),
        Language::Python
    );
    assert_eq!(
        Language::from_path(std::path::Path::new("foo.txt")),
        Language::Unknown
    );
}

#[test]
fn test_parse_rust_function() {
    let source = r#"
fn hello_world() -> String {
    "Hello".to_string()
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}
"#;

    let parsed = ParsedFile::parse(source, Language::Rust).expect("Should parse");
    let functions = parsed.extract_functions();

    assert_eq!(functions.len(), 2);
    assert_eq!(functions[0].name, "hello_world");
    assert_eq!(functions[1].name, "add");
    assert!(functions[1].signature.contains("a"));
    assert!(functions[1].signature.contains("b"));
}

#[cfg(all(
    feature = "lang-rust",
    feature = "lang-python",
    feature = "lang-javascript",
    feature = "lang-typescript"
))]
#[test]
fn test_parse_common_language_functions_and_imports() {
    let cases = [
        (
            Language::Rust,
            r#"
use std::collections::HashMap;

pub async fn load_map() -> HashMap<String, usize> {
    HashMap::new()
}
"#,
            "load_map",
            "std::collections",
        ),
        (
            Language::Python,
            r#"
from pathlib import Path

@pytest.mark.slow
async def load_path(root: Path) -> Path:
    return root / "file.txt"
"#,
            "load_path",
            "pathlib",
        ),
        (
            Language::JavaScript,
            r#"
import fs from "node:fs";

export const readConfig = async (path) => {
    return fs.readFileSync(path, "utf8");
};
"#,
            "readConfig",
            "node:fs",
        ),
        (
            Language::TypeScript,
            r#"
import type { Request } from "./types";

export const handleRequest = (request: Request): string => {
    return request.id;
};
"#,
            "handleRequest",
            "./types",
        ),
    ];

    for (language, source, function_name, import_text) in cases {
        let parsed = ParsedFile::parse(source, language)
            .unwrap_or_else(|| panic!("{language:?} should parse"));
        let functions = parsed.extract_functions();
        assert!(
            functions
                .iter()
                .any(|function| function.name == function_name),
            "{language:?} should extract {function_name}: {functions:?}"
        );
        let imports = parsed.extract_imports();
        assert!(
            imports
                .iter()
                .any(|import| import.raw.contains(import_text)),
            "{language:?} should extract import containing {import_text}: {imports:?}"
        );
    }
}

#[cfg(all(
    feature = "lang-c",
    feature = "lang-cpp",
    feature = "lang-go",
    feature = "lang-java"
))]
#[test]
fn test_parse_extended_language_functions_and_imports() {
    let cases = [
        (
            Language::Go,
            r#"
package main

import "context"

func Serve(ctx context.Context) error {
    return nil
}
"#,
            "Serve",
            "context",
        ),
        (
            Language::Java,
            r#"
import java.util.List;

class Handler {
    public String handle(List<String> values) {
        return values.get(0);
    }
}
"#,
            "handle",
            "java.util.List",
        ),
        (
            Language::C,
            r#"
#include <stdio.h>

int add(int left, int right) {
    return left + right;
}
"#,
            "add",
            "",
        ),
        (
            Language::Cpp,
            r#"
#include <vector>

int sum(std::vector<int> values) {
    return values.size();
}
"#,
            "sum",
            "",
        ),
    ];

    for (language, source, function_name, import_text) in cases {
        let parsed = ParsedFile::parse(source, language)
            .unwrap_or_else(|| panic!("{language:?} should parse"));
        let functions = parsed.extract_functions();
        assert!(
            functions
                .iter()
                .any(|function| function.name == function_name),
            "{language:?} should extract {function_name}: {functions:?}"
        );
        if !import_text.is_empty() {
            let imports = parsed.extract_imports();
            assert!(
                imports
                    .iter()
                    .any(|import| import.raw.contains(import_text)),
                "{language:?} should extract import containing {import_text}: {imports:?}"
            );
        }
    }
}

#[test]
fn test_extract_rust_imports() {
    let source = r#"
use std::collections::HashMap;
use serde::{Deserialize, Serialize};
extern crate anyhow;

fn main() {}
"#;

    let parsed = ParsedFile::parse(source, Language::Rust).expect("Should parse");
    let imports = parsed.extract_imports();

    assert_eq!(imports.len(), 3);
    assert!(imports.iter().any(|i| i.raw.contains("std")));
    assert!(imports.iter().any(|i| i.raw.contains("serde")));
    assert!(imports.iter().any(|i| i.raw.contains("anyhow")));
}

#[test]
fn test_extract_functions_handles_deeply_nested_modules() {
    let source = nested_rust_modules(512, "fn deeply_nested() -> i32 { 42 }");

    let parsed = ParsedFile::parse(source, Language::Rust).expect("Should parse");
    let functions = parsed.extract_functions();

    assert_eq!(functions.len(), 1);
    assert_eq!(functions[0].name, "deeply_nested");
}

#[cfg(feature = "lang-cpp")]
#[test]
fn test_cpp_templated_qualified_function_names() {
    // For `void Foo<U>::bar()` the declarator subtree's first
    // identifier in DFS order is the scope's `type_identifier` ("Foo"),
    // so a plain DFS walk reports every method on the same templated
    // scope as "Foo" and collides them. The fix walks the declarator's
    // `declarator` field and recurses into `qualified_identifier` /
    // `template_function`'s `name` field — see heddle#114 commit
    // dc37af8 for the proven pattern (mirrored from
    // `merge_driver::items::c_function_name`).
    let source = r#"
template <typename U>
struct Foo {
    void bar();
    void baz();
};

template <typename U>
void Foo<U>::bar() {}

template <typename U>
void Foo<U>::baz() {}
"#;

    let parsed = ParsedFile::parse(source, Language::Cpp).expect("Should parse");
    let functions = parsed.extract_functions();
    let names: Vec<&str> = functions.iter().map(|f| f.name.as_str()).collect();

    assert!(
        names.contains(&"bar"),
        "expected templated qualified def to resolve as `bar`, got {names:?}"
    );
    assert!(
        names.contains(&"baz"),
        "expected templated qualified def to resolve as `baz`, got {names:?}"
    );
    let foo_count = names.iter().filter(|n| **n == "Foo").count();
    assert_eq!(
        foo_count, 0,
        "templated qualified method defs should not resolve to scope name `Foo`: {names:?}"
    );
}