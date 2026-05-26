// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_resolve_json_output() {
    let temp = TempDir::new().unwrap();
    create_merge_conflict(&temp);
    fs::write(temp.path().join("file.txt"), "resolved").unwrap();

    let result = heddle(
        &["resolve", "file.txt", "--output", "json"],
        Some(temp.path()),
    );
    assert!(result.is_ok(), "JSON resolve failed: {:?}", result.err());

    let json: Value = serde_json::from_str(&result.unwrap()).expect("valid JSON");
    assert!(json.get("message").is_some(), "should have message field");
    assert!(json.get("resolved").is_some(), "should have resolved field");
}

#[test]
fn test_resolve_no_merge_in_progress() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "file.txt", "content");

    let result = heddle(&["resolve", "file.txt"], Some(temp.path()));
    assert!(result.is_err(), "should fail without merge in progress");
    assert!(
        result.unwrap_err().contains("No merge in progress"),
        "should report no merge"
    );
}

#[test]
fn test_resolve_multiple_conflicting_files() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    fs::write(temp.path().join("a.txt"), "base a").unwrap();
    fs::write(temp.path().join("b.txt"), "base b").unwrap();
    heddle(&["capture", "-m", "Base"], Some(temp.path())).unwrap();

    heddle(&["thread", "create", "feature"], Some(temp.path())).unwrap();
    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "feature a").unwrap();
    fs::write(temp.path().join("b.txt"), "feature b").unwrap();
    heddle(&["capture", "-m", "Feature"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "main"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("a.txt"), "main a").unwrap();
    fs::write(temp.path().join("b.txt"), "main b").unwrap();
    heddle(&["capture", "-m", "Main"], Some(temp.path())).unwrap();

    heddle(&["thread", "switch", "feature"], Some(temp.path())).unwrap();
    let refresh = heddle(
        &["--output", "json", "thread", "refresh", "feature"],
        Some(temp.path()),
    );
    assert!(
        refresh
            .as_ref()
            .is_err_and(|err| err.contains("thread_refresh_conflicted")),
        "refresh should create a durable conflict state: {refresh:?}"
    );

    let result = heddle(&["resolve", "a.txt", "--ours"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "resolve first file failed: {:?}",
        result.err()
    );

    let list_result = heddle(&["resolve", "--list"], Some(temp.path()));
    assert!(
        list_result.unwrap().contains("b.txt"),
        "should show remaining conflict"
    );

    let result = heddle(&["resolve", "--all", "--theirs"], Some(temp.path()));
    assert!(result.is_ok(), "resolve all failed: {:?}", result.err());

    let a_content = fs::read_to_string(temp.path().join("a.txt")).unwrap();
    let b_content = fs::read_to_string(temp.path().join("b.txt")).unwrap();
    assert_eq!(a_content, "feature a", "a.txt should be ours");
    assert_eq!(b_content, "main b", "b.txt should be theirs");
}

#[test]
fn test_resolve_invalid_path() {
    let temp = TempDir::new().unwrap();
    create_merge_conflict(&temp);

    let result = heddle(&["resolve", "nonexistent.txt"], Some(temp.path()));
    assert!(
        result.is_err(),
        "unregistered conflict path should fail closed"
    );
    let error = result.unwrap_err();
    assert!(
        error.contains("No active merge conflict is registered for nonexistent.txt")
            || error.contains("conflict_path_not_found"),
        "should explain the path is not part of the active conflict set: {error}"
    );
}

#[test]
fn test_resolve_refuses_conflict_markers_without_force_or_side_selection() {
    let temp = TempDir::new().unwrap();
    create_merge_conflict(&temp);

    let result = heddle(&["resolve", "file.txt"], Some(temp.path()));
    assert!(
        result.is_err(),
        "manual resolve should not accept conflict markers"
    );
    let error = result.unwrap_err();
    assert!(
        error.contains("conflict markers remain")
            || error.contains("conflict_markers_still_present"),
        "should explain that conflict markers must be removed: {error}"
    );

    let unresolved = heddle(&["resolve", "--list"], Some(temp.path())).unwrap();
    assert!(
        unresolved.contains("file.txt"),
        "failed resolve must leave merge state unresolved: {unresolved}"
    );

    let forced = heddle(&["resolve", "file.txt", "--force"], Some(temp.path()));
    assert!(
        forced.is_ok(),
        "--force should permit an intentional marker-looking resolution: {:?}",
        forced.err()
    );
}

#[test]
fn test_resolve_all_refuses_conflict_markers_without_force_or_side_selection() {
    let temp = TempDir::new().unwrap();
    create_merge_conflict(&temp);

    let result = heddle(&["resolve", "--all"], Some(temp.path()));
    assert!(
        result.is_err(),
        "resolve --all must not mark marker-bearing files resolved"
    );
    let error = result.unwrap_err();
    assert!(
        error.contains("conflict markers remain")
            || error.contains("conflict_markers_still_present"),
        "should explain that conflict markers must be removed: {error}"
    );

    let result = heddle(&["resolve", "--all", "--ours"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "--ours should write a known side and mark the conflict resolved: {:?}",
        result.err()
    );
}

#[test]
fn test_resolve_abort_restores_state() {
    let temp = TempDir::new().unwrap();
    create_merge_conflict(&temp);

    let content_before = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert!(
        content_before.contains("<<<<<<<"),
        "should have conflict markers"
    );

    let result = heddle(&["resolve", "--abort"], Some(temp.path()));
    assert!(result.is_ok(), "abort failed: {:?}", result.err());

    let content_after = fs::read_to_string(temp.path().join("file.txt")).unwrap();
    assert_eq!(
        content_after, "feature content",
        "should restore pre-merge state"
    );
}

#[test]
fn test_resolve_double_resolution() {
    let temp = TempDir::new().unwrap();
    create_merge_conflict(&temp);

    heddle(&["resolve", "file.txt", "--ours"], Some(temp.path())).unwrap();

    let result = heddle(&["resolve", "file.txt", "--ours"], Some(temp.path()));
    assert!(
        result.is_ok(),
        "double resolve should succeed: {:?}",
        result.err()
    );
}
