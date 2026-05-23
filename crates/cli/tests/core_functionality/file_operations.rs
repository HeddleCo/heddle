// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_file_deletion_tracking() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("to_delete.txt"), "content").unwrap();
    heddle_must_succeed(&["capture", "-m", "Add file"], temp.path());
    std::fs::remove_file(temp.path().join("to_delete.txt")).unwrap();
    let result = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(result.contains("deleted") || result.contains("to_delete"));
    heddle_must_succeed(&["capture", "-m", "Delete file"], temp.path());
    assert!(!temp.path().join("to_delete.txt").exists());
}

#[test]
fn test_nested_directory_handling() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::create_dir_all(temp.path().join("src/lib/deep")).unwrap();
    std::fs::write(temp.path().join("src/lib/deep/mod.rs"), "pub fn deep() {}").unwrap();
    std::fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();
    heddle_must_succeed(&["capture", "-m", "Add nested files"], temp.path());
    let result = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(!result.contains("src/lib/deep/mod.rs") || result.contains("clean"));
    std::fs::write(
        temp.path().join("src/lib/deep/mod.rs"),
        "pub fn deep() { modified }",
    )
    .unwrap();
    let result = heddle(&["diff"], Some(temp.path())).unwrap();
    assert!(result.contains("mod.rs") || result.contains("deep"));
}

#[test]
fn test_binary_file_handling() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let binary_content: Vec<u8> = (0..255).collect();
    std::fs::write(temp.path().join("binary.bin"), &binary_content).unwrap();
    heddle_must_succeed(&["capture", "-m", "Add binary"], temp.path());
    let retrieved = std::fs::read(temp.path().join("binary.bin")).unwrap();
    assert_eq!(retrieved, binary_content);
    std::fs::write(temp.path().join("binary.bin"), vec![255u8, 254, 253]).unwrap();
    heddle_must_succeed(&["capture", "-m", "Modify binary"], temp.path());
    heddle_must_succeed(&["goto", "HEAD~1"], temp.path());
    let retrieved = std::fs::read(temp.path().join("binary.bin")).unwrap();
    assert_eq!(retrieved, binary_content);
}

#[test]
fn test_empty_directory_ignored() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::create_dir_all(temp.path().join("empty_dir")).unwrap();
    std::fs::create_dir_all(temp.path().join("dir_with_file")).unwrap();
    std::fs::write(temp.path().join("dir_with_file/file.txt"), "content").unwrap();
    let result = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(result.contains("dir_with_file") || result.contains("file.txt"));
}

#[test]
fn test_multiple_file_modifications() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    for i in 1..=5 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("content {}", i),
        )
        .unwrap();
    }
    heddle_must_succeed(&["capture", "-m", "Initial files"], temp.path());
    for i in 1..=5 {
        std::fs::write(
            temp.path().join(format!("file{}.txt", i)),
            format!("modified {}", i),
        )
        .unwrap();
    }
    let result = heddle(&["diff", "--stat"], Some(temp.path())).unwrap();
    for i in 1..=5 {
        assert!(result.contains(&format!("file{}.txt", i)));
    }
}

#[test]
fn test_symlink_handling() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("target.txt"), "target content").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(temp.path().join("target.txt"), temp.path().join("link.txt"))
        .unwrap();
    heddle_must_succeed(&["capture", "-m", "Add symlink"], temp.path());
    #[cfg(unix)]
    {
        let link_content = std::fs::read_link(temp.path().join("link.txt")).unwrap();
        assert!(
            link_content.ends_with("target.txt")
                || link_content.to_string_lossy().contains("target")
        );
    }
}

#[test]
fn test_nested_tracked_heddle_paths_are_not_ignored_by_status_or_snapshot() {
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    write_nested_tracked_heddle_fixture(temp.path(), "hd-examplehead-v1\n");
    heddle_must_succeed(
        &["capture", "-m", "Thread nested heddle files"],
        temp.path(),
    );
    let status = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let status_json: Value = serde_json::from_str(&status).unwrap();
    assert!(
        status_json["changes"]["deleted"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    std::fs::write(
        temp.path().join("examples/calculator/.heddle/HEAD"),
        "hd-examplehead-v2\n",
    )
    .unwrap();
    let status = heddle(&["status", "--json"], Some(temp.path())).unwrap();
    let status_json: Value = serde_json::from_str(&status).unwrap();
    let modified = status_json["changes"]["modified"].as_array().unwrap();
    assert!(
        modified
            .iter()
            .filter_map(|value| value.as_str())
            .any(|path| path == "examples/calculator/.heddle/HEAD")
    );
    heddle_must_succeed(
        &["capture", "-m", "Update nested heddle files"],
        temp.path(),
    );
    let repo = Repository::open(temp.path()).unwrap();
    let head = repo.current_state().unwrap().unwrap();
    let parent = head.parents[0];
    drop(repo);
    heddle_must_succeed(&["goto", &parent.to_string_full(), "--force"], temp.path());
    let restored =
        std::fs::read_to_string(temp.path().join("examples/calculator/.heddle/HEAD")).unwrap();
    assert_eq!(restored, "hd-examplehead-v1\n");
}

#[test]
fn init_writes_default_heddleignore() {
    // Red-commit for heddle#80: fresh `heddle init` must install the
    // bundled `.heddleignore` so day-one users don't have to discover
    // the file's existence before macOS noise lands.
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let path = temp.path().join(".heddleignore");
    assert!(
        path.is_file(),
        ".heddleignore must be installed by `heddle init`"
    );
    let contents = std::fs::read_to_string(&path).unwrap();
    // Spot-check a few representative patterns from each family —
    // template-completeness is unit-tested in the module itself.
    assert!(contents.contains(".DS_Store"));
    assert!(contents.contains("xcuserdata/"));
    assert!(contents.contains("*.swp"));
}

#[test]
fn init_preserves_existing_heddleignore() {
    // If the operator already curated a `.heddleignore`, init must
    // NOT clobber it. The default template is a starter, not a
    // mandate.
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(".heddleignore");
    std::fs::write(&path, "# my custom rules\n*.private\n").unwrap();
    heddle_must_succeed(&["init"], temp.path());
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("*.private"));
    assert!(
        !after.contains(".DS_Store"),
        "existing .heddleignore must not be overwritten"
    );
}

#[test]
fn default_heddleignore_suppresses_common_macos_noise() {
    // Red-commit: after `heddle init`, dropping `.DS_Store` and an
    // `xcuserdata/` tree into the worktree must NOT show them as
    // untracked. This is the day-one friction the issue cites.
    let temp = TempDir::new().unwrap();
    heddle_must_succeed(&["init"], temp.path());
    std::fs::write(temp.path().join("real.txt"), "content").unwrap();
    std::fs::write(temp.path().join(".DS_Store"), b"\x00\x00\x00").unwrap();
    let xcuserdata = temp.path().join("App.xcodeproj/xcuserdata/u.xcuserdatad");
    std::fs::create_dir_all(&xcuserdata).unwrap();
    std::fs::write(xcuserdata.join("UserInterfaceState.xcuserstate"), b"x").unwrap();

    let status = heddle_must_succeed(&["--json", "status"], temp.path());
    let status_json: Value = serde_json::from_str(&status).unwrap();
    let untracked = status_json["changes"]["added"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let untracked_paths: Vec<&str> = untracked.iter().filter_map(|v| v.as_str()).collect();
    assert!(
        !untracked_paths.iter().any(|p| p.contains(".DS_Store")),
        ".DS_Store must be suppressed by the default .heddleignore; saw: {untracked_paths:?}"
    );
    assert!(
        !untracked_paths.iter().any(|p| p.contains("xcuserdata")),
        "xcuserdata/ must be suppressed by the default .heddleignore; saw: {untracked_paths:?}"
    );
    assert!(
        untracked_paths.iter().any(|p| p.contains("real.txt")),
        "real.txt must still surface as untracked; saw: {untracked_paths:?}"
    );
}
