// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
#[serial]
fn test_merge_rename_on_one_side_modify_on_other() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("foo.rs"),
        "fn main() {\n    println!(\"hello\");\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature-a"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature-a"], Some(temp.path())).unwrap();
    fs::rename(temp.path().join("foo.rs"), temp.path().join("bar.rs")).unwrap();
    heddle(
        &["capture", "-m", "Rename foo.rs to bar.rs"],
        Some(temp.path()),
    )
    .unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("foo.rs"),
        "fn main() {\n    println!(\"hello world\");\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Modify foo.rs"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature-a");
    refresh_then_merge_thread(temp.path(), "feature-a");

    assert!(
        temp.path().join("bar.rs").exists(),
        "renamed file bar.rs should exist"
    );
    let bar_content = fs::read_to_string(temp.path().join("bar.rs")).unwrap();
    assert!(
        bar_content.contains("hello world"),
        "bar.rs should contain the modification from branch B, got: {}",
        bar_content
    );
    assert!(
        !temp.path().join("foo.rs").exists(),
        "original foo.rs should not exist after rename merge"
    );
}

#[test]
#[serial]
fn test_merge_rename_rename_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("shared.rs"), "pub fn shared() {}").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature-a"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature-a"], Some(temp.path())).unwrap();
    fs::rename(temp.path().join("shared.rs"), temp.path().join("alpha.rs")).unwrap();
    heddle(&["capture", "-m", "Rename to alpha"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::rename(temp.path().join("shared.rs"), temp.path().join("beta.rs")).unwrap();
    heddle(&["capture", "-m", "Rename to beta"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature-a");
    let output = refresh_thread_expect_conflict(temp.path(), "feature-a");
    assert!(
        output.contains("conflict") || output.contains("Conflict"),
        "rename/rename should produce a conflict, got: {}",
        output
    );
}

#[test]
#[serial]
fn test_merge_rename_and_modify_same_side() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("foo.rs"),
        "fn process() {\n    step_one();\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature-a"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature-a"], Some(temp.path())).unwrap();
    fs::remove_file(temp.path().join("foo.rs")).unwrap();
    fs::write(
        temp.path().join("bar.rs"),
        "fn process() {\n    step_one();\n    step_two();\n}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Rename and modify"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    let result = heddle(&["merge", "feature-a"], Some(temp.path()));
    assert!(result.is_ok(), "merge should succeed: {:?}", result.err());

    assert!(
        temp.path().join("bar.rs").exists(),
        "renamed file should exist"
    );
    let content = fs::read_to_string(temp.path().join("bar.rs")).unwrap();
    assert!(
        content.contains("step_two"),
        "bar.rs should have the modification"
    );
    assert!(
        !temp.path().join("foo.rs").exists(),
        "original foo.rs should be gone"
    );
}

#[test]
#[serial]
fn test_merge_cross_directory_rename_with_modify() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("src/utils.rs"),
        "pub fn helper() -> u32 { 42 }\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature-a"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature-a"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("src/lib")).unwrap();
    fs::rename(
        temp.path().join("src/utils.rs"),
        temp.path().join("src/lib/utils.rs"),
    )
    .unwrap();
    heddle(&["capture", "-m", "Move utils to lib/"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("src/utils.rs"),
        "pub fn helper() -> u32 { 99 }\npub fn new_fn() {}\n",
    )
    .unwrap();
    heddle(&["capture", "-m", "Modify utils.rs"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature-a");
    refresh_then_merge_thread(temp.path(), "feature-a");

    assert!(
        temp.path().join("src/lib/utils.rs").exists(),
        "moved file should exist at new location"
    );
    let content = fs::read_to_string(temp.path().join("src/lib/utils.rs")).unwrap();
    assert!(
        content.contains("new_fn"),
        "moved file should contain modifications from branch B, got: {}",
        content
    );
}

#[test]
#[serial]
fn test_merge_pure_rename_no_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("foo.rs"), "fn original() {}").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature-a"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature-a"], Some(temp.path())).unwrap();
    fs::rename(temp.path().join("foo.rs"), temp.path().join("bar.rs")).unwrap();
    heddle(&["capture", "-m", "Rename"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("baz.rs"), "fn new_stuff() {}").unwrap();
    heddle(&["capture", "-m", "Add baz"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature-a");
    refresh_then_merge_thread(temp.path(), "feature-a");

    assert!(
        temp.path().join("bar.rs").exists(),
        "renamed file should exist"
    );
    assert!(temp.path().join("baz.rs").exists(), "new file should exist");
    assert!(
        !temp.path().join("foo.rs").exists(),
        "original file should be gone after rename"
    );
}
