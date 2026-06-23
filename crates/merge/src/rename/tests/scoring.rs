// SPDX-License-Identifier: Apache-2.0

use super::*;

#[test]
fn rename_path_similarity_same_file() {
    assert!((path_similarity("src/main.rs", "src/main.rs") - 1.0).abs() < 0.01);
}

#[test]
fn rename_path_similarity_same_filename_different_dir() {
    let sim = path_similarity("src/utils/helpers.rs", "src/lib/helpers.rs");
    assert!(sim > 0.4);
    assert!(sim < 0.8);
}

#[test]
fn rename_path_similarity_completely_different() {
    let sim = path_similarity("src/main.rs", "lib/config.py");
    assert!(sim < 0.1);
}

#[test]
fn rename_delta_similarity_identical() {
    let content = b"fn main() { println!(\"hello\"); }";
    let sim = delta_similarity(content, content);
    assert!((sim - 1.0).abs() < 0.01);
}

#[test]
fn rename_delta_similarity_completely_different() {
    let a = b"fn main() { println!(\"hello world\"); }";
    let b = b"XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX";
    let sim = delta_similarity(a, b);
    assert!(sim < 0.3);
}

#[test]
fn rename_delta_similarity_slightly_modified() {
    let a = b"fn process() {\n    let data = load();\n    transform(data);\n    save(data);\n}\n";
    let b = b"fn process() {\n    let data = load();\n    transform(data);\n    save(data);\n    log(data);\n}\n";
    let sim = delta_similarity(a, b);
    assert!(sim > 0.5);
}
