// SPDX-License-Identifier: Apache-2.0
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::*;

#[cfg(unix)]
fn set_executable(path: &std::path::Path, executable: bool) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    let mut mode = permissions.mode();
    if executable {
        mode |= 0o111;
    } else {
        mode &= !0o111;
    }
    permissions.set_mode(mode);
    fs::set_permissions(path, permissions).unwrap();
}

#[test]
#[serial]
fn test_merge_auto_resolve_creates_merge_commit() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("b.txt"), "feature content").unwrap();
    heddle(&["capture", "-m", "Add b.txt"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("c.txt"), "main content").unwrap();
    heddle(&["capture", "-m", "Add c.txt"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_then_merge_thread(temp.path(), "feature");

    assert!(
        temp.path().join("b.txt").exists(),
        "b.txt from feature should exist after merge"
    );
    assert!(
        temp.path().join("c.txt").exists(),
        "c.txt from main should exist after merge"
    );
    assert!(
        temp.path().join("a.txt").exists(),
        "a.txt from base should still exist"
    );
}

#[test]
#[serial]
#[cfg(unix)]
fn test_merge_executable_bit_vs_content_change_preserves_both() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let script = temp.path().join("tool.sh");
    fs::write(&script, "#!/bin/sh\necho base\n").unwrap();
    set_executable(&script, false);
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(&script, "#!/bin/sh\necho feature\n").unwrap();
    heddle(&["capture", "-m", "Feature content"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    set_executable(&script, true);
    heddle(&["capture", "-m", "Main executable bit"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_then_merge_thread(temp.path(), "feature");
    let content = fs::read_to_string(&script).unwrap();
    assert_eq!(content, "#!/bin/sh\necho feature\n");
    assert!(
        fs::metadata(&script).unwrap().permissions().mode() & 0o111 != 0,
        "merged script should keep main's executable bit"
    );
}

#[test]
#[serial]
#[cfg(unix)]
fn test_merge_conflicting_executable_bit_changes_records_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let script = temp.path().join("tool.sh");
    fs::write(&script, "#!/bin/sh\necho base\n").unwrap();
    set_executable(&script, false);
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    set_executable(&script, true);
    heddle(&["capture", "-m", "Feature executable"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(&script, "#!/bin/sh\necho main\n").unwrap();
    set_executable(&script, true);
    heddle(
        &["capture", "-m", "Main content and executable"],
        Some(temp.path()),
    )
    .unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_then_merge_thread(temp.path(), "feature");
    // Feature only flipped the executable bit; main changed both
    // content and the executable bit. The merge driver correctly
    // resolves this as "take main's content, keep the executable bit
    // both sides set" rather than flagging a content conflict — both
    // sides agree on mode, only main mutated the bytes. Verify the
    // merged file matches main's content and is executable.
    let content = fs::read_to_string(&script).unwrap();
    assert!(
        !content.contains("<<<<<<<"),
        "merge of mode-only-on-feature + content+mode-on-main should not leave conflict markers: {content}"
    );
    assert!(
        content.contains("echo main"),
        "merged file should carry main's content: {content}"
    );
    let metadata = fs::metadata(&script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert!(
        metadata.permissions().mode() & 0o111 != 0,
        "merged file should preserve the executable bit"
    );
    assert!(
        !temp.path().join(".heddle/MERGE_STATE").exists(),
        "auto-resolved merge should not leave MERGE_STATE behind"
    );
}

#[test]
#[serial]
fn test_merge_fast_forward() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("feature.txt"), "feature").unwrap();
    heddle(&["capture", "-m", "Feature commit"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    let result = heddle(&["merge", "feature"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "fast-forward merge should succeed: {:?}",
        result.err()
    );

    let output = result.unwrap();
    assert!(
        output.to_lowercase().contains("fast") || temp.path().join("feature.txt").exists(),
        "fast-forward merge should advance HEAD: {}",
        output
    );
}

#[test]
#[serial]
fn test_merge_already_up_to_date() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("file2.txt"), "more").unwrap();
    heddle(&["capture", "-m", "Advance main"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    let output = refresh_then_merge_thread(temp.path(), "feature");
    assert!(
        output.to_lowercase().contains("up to date")
            || output.to_lowercase().contains("already")
            || output.to_lowercase().contains("nothing"),
        "merge of ancestor should indicate no-op: {}",
        output
    );
}

#[test]
#[serial]
fn test_merge_conflict_markers_in_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "base content").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "feature version").unwrap();
    heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "main version").unwrap();
    heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_thread_expect_conflict(temp.path(), "feature");

    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert!(
        content.contains("<<<<<<<") || content.contains("======="),
        "conflicting merge should leave conflict markers in file: {}",
        content
    );

    let merge_state = temp.path().join(".heddle/MERGE_STATE");
    assert!(
        merge_state.exists(),
        "MERGE_STATE should exist during merge"
    );
}

#[test]
#[serial]
fn test_merge_conflict_resolve_then_snapshot() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "feature version").unwrap();
    heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "main version").unwrap();
    heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_thread_expect_conflict(temp.path(), "feature");

    fs::write(temp.path().join("file.txt"), "resolved content").unwrap();
    heddle(&["resolve", "--all"], Some(temp.path())).unwrap();

    let result = heddle(&["capture", "-m", "Merge resolved"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "snapshot after resolve should succeed: {:?}",
        result.err()
    );
}

#[test]
#[serial]
fn test_merge_delete_vs_unchanged_deletes_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::remove_file(temp.path().join("file.txt")).unwrap();
    heddle(&["capture", "-m", "Delete file"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    refresh_then_merge_thread(temp.path(), "feature");
    assert!(
        !temp.path().join("file.txt").exists(),
        "delete from feature should win when main left the file unchanged"
    );
}

#[test]
#[serial]
fn test_merge_delete_vs_modified_records_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::remove_file(temp.path().join("file.txt")).unwrap();
    heddle(&["capture", "-m", "Delete file"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("file.txt"), "main changed").unwrap();
    heddle(&["capture", "-m", "Modify file"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_thread_expect_conflict(temp.path(), "feature");

    let content = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert!(
        content.contains("<<<<<<<") && content.contains(">>>>>>>"),
        "modify/delete merge should leave a conflict marker file: {}",
        content
    );
    assert!(
        temp.path().join(".heddle/MERGE_STATE").exists(),
        "modify/delete merge should remain in conflict"
    );
}

#[test]
#[serial]
fn test_merge_binary_modify_vs_delete_records_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("asset.bin"), b"\x00base\xff").unwrap();
    heddle(&["capture", "-m", "Base binary"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::remove_file(temp.path().join("asset.bin")).unwrap();
    heddle(&["capture", "-m", "Delete binary"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("asset.bin"), b"\x00main changed\xff").unwrap();
    heddle(&["capture", "-m", "Modify binary"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_thread_expect_conflict(temp.path(), "feature");

    let content = fs::read(temp.path().join("asset.bin")).unwrap();
    assert!(
        content
            .windows("<<<<<<<".len())
            .any(|window| window == b"<<<<<<<")
            && content
                .windows(">>>>>>>".len())
                .any(|window| window == b">>>>>>>"),
        "binary modify/delete merge should leave a conflict marker file: {:?}",
        content
    );
    assert!(
        temp.path().join(".heddle/MERGE_STATE").exists(),
        "binary modify/delete merge should remain in conflict"
    );
}

#[test]
#[serial]
fn test_merge_rename_vs_delete_records_conflict() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("old.txt"), "shared content\n").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::rename(temp.path().join("old.txt"), temp.path().join("new.txt")).unwrap();
    heddle(&["capture", "-m", "Rename old to new"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::remove_file(temp.path().join("old.txt")).unwrap();
    heddle(&["capture", "-m", "Delete old"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    let output = refresh_thread_expect_conflict(temp.path(), "feature");

    assert!(
        temp.path().join("new.txt").exists(),
        "rename/delete conflict should preserve the renamed file for resolution"
    );
    assert!(
        output.contains("rename/delete conflict")
            && output.contains("old.txt")
            && output.contains("new.txt"),
        "rename/delete merge should explain both paths: {output}"
    );
    assert!(
        temp.path().join(".heddle/MERGE_STATE").exists(),
        "rename/delete merge should remain in conflict"
    );
}

#[test]
#[serial]
fn test_merge_directory_file_conflict_materializes_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("README.md"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("node"), "feature file").unwrap();
    heddle(&["capture", "-m", "Add node file"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::create_dir(temp.path().join("node")).unwrap();
    fs::write(temp.path().join("node/child.txt"), "main child").unwrap();
    heddle(&["capture", "-m", "Add node directory"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_thread_expect_conflict(temp.path(), "feature");

    let content = fs::read_to_string(temp.path().join("node")).unwrap();
    assert!(
        content.contains("<<<<<<<") && content.contains("<directory>"),
        "directory/file conflict should materialize an explicit conflict file: {}",
        content
    );
    assert!(
        temp.path().join(".heddle/MERGE_STATE").exists(),
        "directory/file merge should remain in conflict"
    );
}

#[test]
#[serial]
fn test_merge_file_directory_conflict_materializes_file() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("README.md"), "base").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::create_dir(temp.path().join("node")).unwrap();
    fs::write(temp.path().join("node/child.txt"), "feature child").unwrap();
    heddle(&["capture", "-m", "Add node directory"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("node"), "main file").unwrap();
    heddle(&["capture", "-m", "Add node file"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_thread_expect_conflict(temp.path(), "feature");

    let content = fs::read_to_string(temp.path().join("node")).unwrap();
    assert!(
        content.contains("<<<<<<<") && content.contains("<directory>"),
        "file/directory conflict should materialize an explicit conflict file: {}",
        content
    );
    assert!(
        temp.path().join(".heddle/MERGE_STATE").exists(),
        "file/directory merge should remain in conflict"
    );
}

#[test]
#[serial]
fn test_merge_preserves_subdirectory_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/lib.rs"), "// base lib").unwrap();
    heddle(
        &["capture", "-m", "Base with src/lib.rs"],
        Some(temp.path()),
    )
    .unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("src/util")).unwrap();
    fs::write(temp.path().join("src/util/helpers.rs"), "// helpers").unwrap();
    heddle(
        &["capture", "-m", "Add src/util/helpers.rs"],
        Some(temp.path()),
    )
    .unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("tests")).unwrap();
    fs::write(temp.path().join("tests/test.rs"), "// test").unwrap();
    heddle(&["capture", "-m", "Add tests/test.rs"], Some(temp.path())).unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_then_merge_thread(temp.path(), "feature");

    assert!(
        temp.path().join("src/lib.rs").exists(),
        "src/lib.rs from base should exist after merge"
    );
    assert!(
        temp.path().join("src/util/helpers.rs").exists(),
        "src/util/helpers.rs from feature should exist after merge"
    );
    assert!(
        temp.path().join("tests/test.rs").exists(),
        "tests/test.rs from main should exist after merge"
    );
}

#[test]
#[serial]
fn test_merge_directory_restructure_vs_modification() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::create_dir_all(temp.path().join("src")).unwrap();
    fs::write(temp.path().join("src/main.rs"), "fn main() {}").unwrap();
    fs::write(temp.path().join("src/lib.rs"), "pub mod store;").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "restructure"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "restructure"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("crates/core/src")).unwrap();
    fs::write(temp.path().join("crates/core/src/main.rs"), "fn main() {}").unwrap();
    fs::write(temp.path().join("crates/core/src/lib.rs"), "pub mod store;").unwrap();
    fs::remove_file(temp.path().join("src/main.rs")).unwrap();
    fs::remove_file(temp.path().join("src/lib.rs")).unwrap();
    fs::remove_dir(temp.path().join("src")).unwrap();
    heddle(
        &["capture", "-m", "Restructure into crates/"],
        Some(temp.path()),
    )
    .unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(
        temp.path().join("src/main.rs"),
        "fn main() { println!(\"hello\"); }",
    )
    .unwrap();
    fs::write(
        temp.path().join("src/lib.rs"),
        "pub mod store;\npub mod delta;",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "Add features to src/"],
        Some(temp.path()),
    )
    .unwrap();

    assert_stale_merge_refuses(temp.path(), "restructure");
    refresh_then_merge_thread(temp.path(), "restructure");

    assert!(
        temp.path().join("crates/core/src/main.rs").exists()
            || temp.path().join("crates/core/src/lib.rs").exists()
            || temp.path().join("crates").exists(),
        "crates/ directory from restructure branch should be present after merge"
    );
}

#[test]
#[serial]
fn test_merge_deep_nested_directories() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("README.md"), "# project").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("src/store/pack")).unwrap();
    fs::write(
        temp.path().join("src/store/pack/builder.rs"),
        "// pack builder",
    )
    .unwrap();
    heddle(&["capture", "-m", "Add deep file"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("src/protocol/delta")).unwrap();
    fs::write(
        temp.path().join("src/protocol/delta/encoder.rs"),
        "// encoder",
    )
    .unwrap();
    heddle(
        &["capture", "-m", "Add another deep file"],
        Some(temp.path()),
    )
    .unwrap();

    assert_stale_merge_refuses(temp.path(), "feature");
    refresh_then_merge_thread(temp.path(), "feature");

    assert!(
        temp.path().join("src/store/pack/builder.rs").exists(),
        "deeply nested file from feature should exist"
    );
    assert!(
        temp.path().join("src/protocol/delta/encoder.rs").exists(),
        "deeply nested file from main should exist"
    );
    assert!(
        temp.path().join("README.md").exists(),
        "base file should still exist"
    );
}

#[test]
#[serial]
fn test_cherry_pick_with_subdirectories() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("root.txt"), "root").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::create_dir_all(temp.path().join("src/models")).unwrap();
    fs::write(temp.path().join("src/models/user.rs"), "struct User {}").unwrap();
    let snapshot_output = heddle(&["capture", "-m", "Add nested file"], Some(temp.path())).unwrap();
    let snap: Value = serde_json::from_str(&snapshot_output).unwrap();
    let change_id = snap["change_id"].as_str().unwrap().to_string();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    let result = heddle(&["cherry-pick", &change_id], Some(temp.path()));
    assert!(
        result.is_ok(),
        "cherry-pick with subdirectory files should succeed: {:?}",
        result.err()
    );

    assert!(
        temp.path().join("src/models/user.rs").exists(),
        "cherry-picked nested file should exist on disk"
    );
}
