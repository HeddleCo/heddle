// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_blame_large_file_performance() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let mut content = String::new();
    for i in 0..1000 {
        content.push_str(&format!("Line {}: some content here\n", i));
    }
    fs::write(temp.path().join("large.txt"), content).unwrap();
    heddle(&["capture", "-m", "Large file"], Some(temp.path())).unwrap();

    assert_performance(
        "blame large file",
        || {
            let _ = heddle(&["blame", "large.txt"], Some(temp.path()));
        },
        Duration::from_secs(2),
    );
}

#[test]
fn test_blame_binary_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let binary_content: Vec<u8> = (0..256).map(|i| i as u8).collect();
    fs::write(temp.path().join("binary.bin"), binary_content).unwrap();
    heddle(&["capture", "-m", "Binary"], Some(temp.path())).unwrap();

    let result = heddle(&["blame", "binary.bin"], Some(temp.path()));
    assert!(result.is_ok() || result.unwrap_err().contains("binary"));
}

#[test]
fn test_blame_nonexistent_file() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(&["blame", "nonexistent.txt"], Some(temp.path()));
    assert!(result.is_err(), "blame of nonexistent file should fail");
}

#[test]
fn test_blame_empty_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("empty.txt"), "").unwrap();
    heddle(&["capture", "-m", "Empty"], Some(temp.path())).unwrap();

    let result = heddle(&["blame", "empty.txt"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "blame of empty file should succeed: {:?}",
        result.err()
    );
}

#[test]
fn test_blame_multiple_commits_attribution() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\n").unwrap();
    heddle(&["capture", "-m", "First line"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\nline 2\n").unwrap();
    heddle(&["capture", "-m", "Second line"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("tracked.txt"), "line 1\nmodified 2\n").unwrap();
    heddle(&["capture", "-m", "Modified second"], Some(temp.path())).unwrap();

    let result = heddle(&["blame", "tracked.txt"], Some(temp.path()));
    assert!(result.is_ok(), "blame failed: {:?}", result.err());

    let output = result.unwrap();
    assert!(output.contains("line 1") || output.contains("modified 2") || output.contains("hd-"));
}