// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
#[serial]
fn test_merge_json_output_includes_renames() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("old.rs"), "fn original() {}").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::rename(temp.path().join("old.rs"), temp.path().join("new.rs")).unwrap();
    heddle(&["capture", "-m", "Rename"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("other.txt"), "other content").unwrap();
    heddle(&["capture", "-m", "Add other"], Some(temp.path())).unwrap();

    let result = heddle(&["merge", "feature", "--output", "json"], Some(temp.path()));
    let err = result.expect_err("stale JSON merge should refuse before producing rename output");
    assert!(
        err.contains("\"status\":\"blocked\"")
            && err.contains("\"recommended_action\":\"heddle thread refresh feature\""),
        "stale JSON merge should recommend thread refresh: {err}"
    );

    refresh_then_merge_thread(temp.path(), "feature");
    assert!(temp.path().join("new.rs").exists());
    assert!(temp.path().join("other.txt").exists());
}

#[test]
#[serial]
fn test_merge_text_output_shows_rename_lines() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("old.rs"), "fn original() {}").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::rename(temp.path().join("old.rs"), temp.path().join("new.rs")).unwrap();
    heddle(&["capture", "-m", "Rename"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("other.txt"), "other").unwrap();
    heddle(&["capture", "-m", "Add other"], Some(temp.path())).unwrap();

    let result = heddle(&["merge", "feature", "--output", "text"], Some(temp.path()));
    let err = result.expect_err("stale text merge should refuse before producing rename lines");
    assert!(
        err.contains("Thread 'feature' is stale") && err.contains("heddle thread refresh feature"),
        "stale text merge should recommend thread refresh: {err}"
    );

    refresh_then_merge_thread(temp.path(), "feature");
    assert!(temp.path().join("new.rs").exists());
    assert!(temp.path().join("other.txt").exists());
}

#[test]
#[serial]
fn test_merge_json_output_includes_directory_renames() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::create_dir_all(temp.path().join("src/old_module")).unwrap();
    fs::write(temp.path().join("src/old_module/a.rs"), "fn a() {}").unwrap();
    fs::write(temp.path().join("src/old_module/b.rs"), "fn b() {}").unwrap();
    fs::write(temp.path().join("src/old_module/c.rs"), "fn c() {}").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("src/new_module")).unwrap();
    fs::rename(
        temp.path().join("src/old_module/a.rs"),
        temp.path().join("src/new_module/a.rs"),
    )
    .unwrap();
    fs::rename(
        temp.path().join("src/old_module/b.rs"),
        temp.path().join("src/new_module/b.rs"),
    )
    .unwrap();
    fs::rename(
        temp.path().join("src/old_module/c.rs"),
        temp.path().join("src/new_module/c.rs"),
    )
    .unwrap();
    fs::remove_dir(temp.path().join("src/old_module")).unwrap();
    heddle(&["capture", "-m", "Rename module"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("README.md"), "readme").unwrap();
    heddle(&["capture", "-m", "Add readme"], Some(temp.path())).unwrap();

    let result = heddle(&["merge", "feature", "--output", "json"], Some(temp.path()));
    let err = result.expect_err("stale directory rename merge should refuse before metadata");
    assert!(
        err.contains("\"status\":\"blocked\"")
            && err.contains("\"recommended_action\":\"heddle thread refresh feature\""),
        "stale JSON merge should recommend thread refresh: {err}"
    );

    refresh_then_merge_thread(temp.path(), "feature");
    assert!(temp.path().join("src/new_module/a.rs").exists());
    assert!(temp.path().join("README.md").exists());
}

#[test]
#[serial]
fn test_merge_no_renames_omits_field_from_json() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("c.txt"), "main").unwrap();
    heddle(&["capture", "-m", "Add c"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("b.txt"), "feature").unwrap();
    heddle(&["capture", "-m", "Add b"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();

    let result = heddle(&["merge", "feature", "--output", "json"], Some(temp.path()));
    assert!(result.is_ok(), "merge should succeed: {:?}", result.err());

    let output: Value = serde_json::from_str(&result.unwrap()).unwrap();
    assert!(
        output.get("renames").is_none(),
        "renames should be omitted when empty, got: {}",
        serde_json::to_string_pretty(&output).unwrap()
    );
}
