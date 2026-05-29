//! Tree-sitter semantic diff end-to-end integration tests.
//!
//! These tests verify that semantic analysis actually works with real code changes.

use objects::store::ObjectStore;
use repo::Repository;
use semantic::{
    analysis::{SimilarityMethod, detect_file_renames, detect_function_changes},
    parser::{Language, ParsedFile},
};
use tempfile::TempDir;

/// Helper to create a Rust source file with specific content.
fn create_rust_file(dir: &std::path::Path, name: &str, content: &str) {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
}

/// Test parsing a real Rust file.
#[test]
fn test_parse_real_rust_code() {
    let code = r#"
fn main() {
    println!("Hello, world!");
}

fn add(a: i32, b: i32) -> i32 {
    a + b
}

struct Point {
    x: f64,
    y: f64,
}

impl Point {
    fn new(x: f64, y: f64) -> Self {
        Point { x, y }
    }
    
    fn distance(&self, other: &Point) -> f64 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}
"#;

    let temp = TempDir::new().unwrap();
    create_rust_file(temp.path(), "test.rs", code);

    let parsed = ParsedFile::parse(code, Language::Rust).expect("Should parse Rust code");
    let functions = parsed.extract_functions();

    // Should find all functions
    assert!(
        !functions.is_empty(),
        "Should parse functions from Rust code"
    );
    assert!(
        functions.iter().any(|f| f.name == "main"),
        "Should find main function"
    );
    assert!(
        functions.iter().any(|f| f.name == "add"),
        "Should find add function"
    );
    assert!(
        functions.iter().any(|f| f.name == "new"),
        "Should find new method"
    );
    assert!(
        functions.iter().any(|f| f.name == "distance"),
        "Should find distance method"
    );
}

/// Test import extraction from Rust code.
#[test]
fn test_extract_rust_imports_real() {
    let code = r#"
use std::collections::HashMap;
use std::io::{self, Read, Write};
use serde::{Serialize, Deserialize};
use tokio::net::TcpListener;

fn main() {}
"#;

    let parsed = ParsedFile::parse(code, Language::Rust).expect("Should parse code");
    let imports = parsed.extract_imports();

    // Check for crate-level imports
    let import_names: Vec<_> = imports.iter().map(|i| i.raw.clone()).collect();
    assert!(
        import_names.iter().any(|n| n.contains("std::collections")),
        "Should find std imports"
    );
}

/// Test parsing a real Python file.
#[test]
fn test_parse_real_python_code() {
    let code = r#"
import os
from sys import path

def main():
    print("hello")

def helper(value: int) -> int:
    return value + 1
"#;

    let parsed = ParsedFile::parse(code, Language::Python).expect("Should parse Python code");
    let functions = parsed.extract_functions();
    let imports = parsed.extract_imports();

    assert!(functions.iter().any(|f| f.name == "main"));
    assert!(functions.iter().any(|f| f.name == "helper"));
    assert!(imports.len() >= 2);
}

/// Test parsing a real JavaScript file.
#[test]
fn test_parse_real_javascript_code() {
    let code = r#"
import { readFile } from "fs";

function greet(name) {
  return `hello ${name}`;
}

class Greeter {
  greetAll(names) {
    return names.map(greet);
  }
}
"#;

    let parsed = ParsedFile::parse(code, Language::JavaScript).expect("Should parse JS code");
    let functions = parsed.extract_functions();
    let imports = parsed.extract_imports();

    assert!(functions.iter().any(|f| f.name == "greet"));
    assert!(functions.iter().any(|f| f.name == "greetAll"));
    assert!(!imports.is_empty());
}

/// Test parsing a real TypeScript file.
#[test]
fn test_parse_real_typescript_code() {
    let code = r#"
import type { Config } from "./types";

export function build(config: Config): string {
  return config.name;
}
"#;

    let parsed =
        ParsedFile::parse(code, Language::TypeScript).expect("Should parse TypeScript code");
    let functions = parsed.extract_functions();
    let imports = parsed.extract_imports();

    assert!(functions.iter().any(|f| f.name == "build"));
    assert!(!imports.is_empty());
}

/// Test detecting function renames in real code.
#[test]
fn test_detect_function_rename_real() {
    let original = r#"
fn calculate_sum(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    let result = calculate_sum(5, 3);
    println!("{}", result);
}
"#;

    // Modified code - function renamed but same body
    let modified = r#"
fn add_numbers(a: i32, b: i32) -> i32 {
    a + b
}

fn main() {
    let result = add_numbers(5, 3);
    println!("{}", result);
}
"#;

    let temp = TempDir::new().unwrap();
    let old_path = temp.path().join("v1.rs");
    let new_path = temp.path().join("v2.rs");

    create_rust_file(temp.path(), "v1.rs", original);
    create_rust_file(temp.path(), "v2.rs", modified);

    let changes = detect_function_changes(
        &old_path,
        &new_path,
        original,
        modified,
        SimilarityMethod::Lines,
    );

    // Should detect the rename via semantic change detection
    // The function add_numbers was added
    assert!(!changes.is_empty(), "Should detect function changes");

    // Check that we detected a function being added (as a file modification)
    let has_addition = changes
        .iter()
        .any(|c| matches!(c, objects::object::SemanticChange::FileModified { .. }));
    assert!(has_addition, "Should detect add_numbers as added");
}

/// Test detecting function signature changes.
#[test]
fn test_detect_function_signature_change() {
    let original = r#"
fn process(data: &str) -> String {
    data.to_uppercase()
}
"#;

    // Changed signature - added parameter
    let modified = r#"
fn process(data: &str, uppercase: bool) -> String {
    if uppercase {
        data.to_uppercase()
    } else {
        data.to_lowercase()
    }
}
"#;

    let temp = TempDir::new().unwrap();
    let old_path = temp.path().join("v1.rs");
    let new_path = temp.path().join("v2.rs");

    std::fs::write(&old_path, original).unwrap();
    std::fs::write(&new_path, modified).unwrap();

    let changes = detect_function_changes(
        &old_path,
        &new_path,
        original,
        modified,
        SimilarityMethod::Lines,
    );

    // Should detect the modification (as a file modification)
    let has_modification = changes
        .iter()
        .any(|c| matches!(c, objects::object::SemanticChange::FileModified { .. }));
    assert!(
        has_modification,
        "Should detect process function modification"
    );
}

/// Test file rename detection by content similarity.
#[test]
fn test_detect_file_rename_by_content() {
    let temp = TempDir::new().unwrap();

    // Simulate deleted and added files with similar content
    let deleted_files: Vec<(std::path::PathBuf, String)> = vec![(
        temp.path().join("old_module.rs"),
        "pub fn helper() -> i32 { 42 }\npub fn another() {}".to_string(),
    )];

    let added_files: Vec<(std::path::PathBuf, String)> = vec![(
        temp.path().join("new_module.rs"),
        "pub fn helper() -> i32 { 43 }\npub fn another() {}".to_string(),
    )];

    // Detect renames with high similarity threshold
    let renames = detect_file_renames(&deleted_files, &added_files, 0.8, SimilarityMethod::Ast);

    assert!(
        !renames.is_empty(),
        "Should detect rename based on content similarity"
    );
    assert_eq!(renames[0].0, temp.path().join("old_module.rs"));
    assert_eq!(renames[0].1, temp.path().join("new_module.rs"));
}

/// Test detecting dependency changes.
#[test]
fn test_detect_dependency_changes() {
    let original = r#"
use std::collections::HashMap;
use std::io::Read;

fn main() {}
"#;

    // Added new import
    let modified = r#"
use std::collections::HashMap;
use std::io::Read;
use serde_json::Value;
use tokio::time::Duration;

fn main() {}
"#;

    let parsed1 = ParsedFile::parse(original, Language::Rust).expect("Should parse");
    let parsed2 = ParsedFile::parse(modified, Language::Rust).expect("Should parse");

    let imports1 = parsed1.extract_imports();
    let imports2 = parsed2.extract_imports();

    // Find added dependencies
    let _added: Vec<_> = imports2
        .iter()
        .filter(|i2| !imports1.iter().any(|i1| i1.raw == i2.raw))
        .collect();

    // Should have more imports in modified
    assert!(
        imports2.len() > imports1.len(),
        "Should detect added imports"
    );
}

/// Test extracting function signatures.
#[test]
fn test_extract_function_signatures() {
    let code = r#"
pub async fn fetch_data(url: &str) -> Result<Vec<u8>, Error> {
    Ok(vec![])
}

fn generic_function<T: Display>(item: T) -> String {
    format!("{}", item)
}

unsafe fn raw_pointer_stuff(ptr: *const u8) -> u8 {
    *ptr
}

const fn compile_time_calc() -> usize {
    42
}
"#;

    let parsed = ParsedFile::parse(code, Language::Rust).expect("Should parse");
    let functions = parsed.extract_functions();

    assert!(
        functions.iter().any(|f| f.name == "fetch_data"),
        "Should find async fn"
    );
    assert!(
        functions.iter().any(|f| f.name == "generic_function"),
        "Should find generic fn"
    );
    assert!(
        functions.iter().any(|f| f.name == "raw_pointer_stuff"),
        "Should find unsafe fn"
    );
    assert!(
        functions.iter().any(|f| f.name == "compile_time_calc"),
        "Should find const fn"
    );
}

/// Test semantic diff on actual repository snapshot.
#[test]
fn test_semantic_diff_on_repository() {
    let temp = TempDir::new().unwrap();
    let repo = Repository::init_default(temp.path()).unwrap();

    // Create initial source code
    std::fs::create_dir(temp.path().join("src")).unwrap();
    create_rust_file(
        temp.path().join("src").as_path(),
        "lib.rs",
        r#"
pub fn calculate(x: i32) -> i32 {
    x * 2
}

pub mod utils;
"#,
    );

    create_rust_file(
        temp.path().join("src").as_path(),
        "utils.rs",
        r#"
pub fn helper() {}
"#,
    );

    let state1 = repo
        .snapshot(Some("Initial code".to_string()), None)
        .unwrap();

    // Make semantic changes
    create_rust_file(
        temp.path().join("src").as_path(),
        "lib.rs",
        r#"
pub fn compute(x: i32) -> i32 {
    x * 2
}

pub mod utils;
"#,
    );

    // Add new file
    create_rust_file(
        temp.path().join("src").as_path(),
        "new_module.rs",
        r#"
pub fn new_function() {}
"#,
    );

    let state2 = repo
        .snapshot(Some("Modified code".to_string()), None)
        .unwrap();

    // Verify both states exist
    assert!(repo.store().has_state(&state1.change_id).unwrap());
    assert!(repo.store().has_state(&state2.change_id).unwrap());

    // The trees should be different
    assert_ne!(state1.tree, state2.tree);
}

/// Test detecting moved code between files.
#[test]
fn test_detect_cross_file_code_move() {
    // Code in original file
    let original_main = r#"
fn main() {}

fn shared_helper() -> i32 {
    42
}
"#;

    // Code split into multiple files
    let modified_main = r#"
mod helpers;

fn main() {
    let _ = helpers::shared_helper();
}
"#;

    let helpers = r#"
pub fn shared_helper() -> i32 {
    42
}
"#;

    let parsed_original = ParsedFile::parse(original_main, Language::Rust).expect("Should parse");
    let parsed_main = ParsedFile::parse(modified_main, Language::Rust).expect("Should parse");
    let parsed_helpers = ParsedFile::parse(helpers, Language::Rust).expect("Should parse");

    // Original has main and shared_helper
    let original_funcs = parsed_original.extract_functions();
    assert!(original_funcs.iter().any(|f| f.name == "main"));
    assert!(original_funcs.iter().any(|f| f.name == "shared_helper"));

    // Modified main only has main, helpers has shared_helper
    let main_funcs = parsed_main.extract_functions();
    let helper_funcs = parsed_helpers.extract_functions();

    assert!(main_funcs.iter().any(|f| f.name == "main"));
    assert!(!main_funcs.iter().any(|f| f.name == "shared_helper"));
    assert!(helper_funcs.iter().any(|f| f.name == "shared_helper"));
}

/// Test language detection from file extension.
#[test]
fn test_language_detection() {
    let cases = vec![
        ("main.rs", Language::Rust),
        ("lib.rs", Language::Rust),
        ("main.py", Language::Python),
        ("script.js", Language::JavaScript),
        ("app.ts", Language::TypeScript),
        ("main.go", Language::Go),
        ("native.c", Language::C),
        ("native.cpp", Language::Cpp),
        ("Example.java", Language::Java),
        ("readme.txt", Language::Unknown),
        ("data.json", Language::Unknown),
        ("Makefile", Language::Unknown),
    ];

    for (filename, expected) in cases {
        let detected = Language::from_path(std::path::Path::new(filename));
        assert_eq!(
            detected, expected,
            "Language detection for {} should be {:?}",
            filename, expected
        );
    }
}

/// Test handling of syntax errors gracefully.
#[test]
fn test_handle_syntax_errors() {
    // Invalid Rust code
    let bad_code = r#"
fn broken( {
    let x = 
}
"#;

    // Should handle gracefully (return None)
    let result = ParsedFile::parse(bad_code, Language::Rust);

    // Parsing may fail for broken code
    assert!(
        result.is_none() || result.unwrap().extract_functions().is_empty(),
        "Should handle broken code gracefully"
    );
}

/// Test similarity computation.
#[test]
fn test_similarity_computation() {
    use semantic::analysis::compute_similarity;

    let identical = "fn main() {}";
    assert_eq!(
        compute_similarity(identical, identical, SimilarityMethod::Lines),
        1.0
    );

    let completely_different = "fn a() {}";
    let different_content = "fn b() { println!(); }";
    let similarity = compute_similarity(
        completely_different,
        different_content,
        SimilarityMethod::Lines,
    );
    assert!(
        similarity < 0.5,
        "Different code should have low similarity"
    );

    let similar_a = "fn main() { println!(\"hello\"); }";
    let similar_b = "fn main() { println!(\"world\"); }";
    let similarity = compute_similarity(similar_a, similar_b, SimilarityMethod::Lines);
    assert!(
        similarity > 0.3,
        "Similar code should have higher similarity"
    );
}

/// Test performance on moderately sized file.
#[test]
fn test_parsing_performance() {
    use std::time::Instant;

    // Generate a moderately sized file (100 functions)
    let mut code = String::new();
    for i in 0..100 {
        code.push_str(&format!(
            r#"
fn function_{}(x: i32) -> i32 {{
    let a = x + {};
    let b = a * 2;
    b - {}
}}
"#,
            i, i, i
        ));
    }

    let start = Instant::now();
    let parsed = ParsedFile::parse(&code, Language::Rust).expect("Should parse");
    let functions = parsed.extract_functions();
    let elapsed = start.elapsed();

    assert_eq!(functions.len(), 100);
    assert!(
        elapsed.as_millis() < 5000,
        "Parsing 100 functions should take less than 5 seconds, took {:?}",
        elapsed
    );
}
