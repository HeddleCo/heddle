// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_path_with_spaces() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("file with spaces.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Spaces"], Some(temp.path())).unwrap();

    let result = heddle(&["status"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "spaces in filename should work: {:?}",
        result.err()
    );
}

#[test]
fn test_nested_directories() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let deep_path = temp.path().join("a/b/c/d/e/f");
    fs::create_dir_all(&deep_path).unwrap();
    fs::write(deep_path.join("deep.txt"), "deep content").unwrap();

    let result = heddle(&["capture", "-m", "Deep"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "deep nesting should work: {:?}",
        result.err()
    );

    let status = status_json(temp.path());
    assert!(status.get("changes").is_some(), "status should work");
}

#[test]
fn test_path_case_sensitivity() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("File.txt"), "upper").unwrap();
    fs::write(temp.path().join("file.txt"), "lower").unwrap();

    let result = heddle(&["capture", "-m", "Case"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "case sensitivity should work: {:?}",
        result.err()
    );

    let status = heddle(&["status", "--output", "text"], Some(temp.path())).unwrap();
    assert!(status.contains("clean") || status.contains("Nothing"));
}

#[test]
fn test_hidden_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join(".hidden"), "secret").unwrap();
    fs::write(temp.path().join("visible"), "normal").unwrap();

    let result = heddle(&["capture", "-m", "Hidden"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "hidden files should work: {:?}",
        result.err()
    );

    let heddle_dir = temp.path().join(".heddle");
    assert_exists(&heddle_dir, ".heddle should exist");
}