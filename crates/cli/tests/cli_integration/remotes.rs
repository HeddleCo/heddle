// SPDX-License-Identifier: Apache-2.0
use super::*;

#[test]
fn test_cli_remote_operations() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let result = heddle(
        &["remote", "add", "origin", "localhost:8421"],
        Some(temp.path()),
    );
    assert!(result.is_ok(), "Remote add failed: {:?}", result.err());
    assert!(
        result.as_ref().unwrap().contains("added remote origin"),
        "Remote add should confirm creation: {:?}",
        result.as_ref().ok()
    );

    let output = heddle(&["remote", "list"], Some(temp.path())).unwrap();
    assert!(
        output.contains("origin") && output.contains("localhost:8421"),
        "Should list added remote: {}",
        output
    );

    let output = heddle(&["remote", "show", "origin"], Some(temp.path())).unwrap();
    assert!(
        output.contains("origin") && output.contains("localhost:8421"),
        "Remote show should include details: {}",
        output
    );

    let result = heddle(&["remote", "remove", "origin"], Some(temp.path()));
    assert!(result.is_ok(), "Remote remove failed: {:?}", result.err());
    assert!(
        result.as_ref().unwrap().contains("removed remote origin"),
        "Remote remove should confirm deletion: {:?}",
        result.as_ref().ok()
    );

    let result = heddle(&["remote", "list"], Some(temp.path())).unwrap();
    assert!(
        result.contains("No remotes configured"),
        "empty remote list should advertise the empty state: {result}"
    );
}

#[test]
fn test_cli_pull_local_updates_requested_track() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("hello.txt"), "from source").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();

    heddle(&["init"], Some(target.path())).unwrap();

    let source_path = source.path().to_str().unwrap().to_string();
    let output = heddle(
        &[
            "pull",
            &source_path,
            "--thread",
            "main",
            "--local-thread",
            "imported",
        ],
        Some(target.path()),
    )
    .unwrap();
    assert!(
        output.contains("\"success\":true"),
        "pull should report success: {}",
        output
    );

    let target_repo = Repository::open(target.path()).unwrap();
    assert!(
        target_repo.refs().get_thread("imported").unwrap().is_some(),
        "imported thread should be created"
    );
    heddle(&["thread", "switch", "imported"], Some(target.path())).unwrap();
    let blob = std::fs::read_to_string(target.path().join("hello.txt")).unwrap();
    assert_eq!(blob, "from source");
}

#[test]
fn test_cli_clone_help_lists_lazy_flag() {
    let output = heddle(&["clone", "--help"], None).unwrap();
    assert!(
        output.contains("--lazy"),
        "clone help should advertise first-class lazy clone support: {output}"
    );
}

#[test]
fn test_cli_pull_help_lists_lazy_flag() {
    let output = heddle(&["pull", "--help"], None).unwrap();
    assert!(
        output.contains("--lazy"),
        "pull help should advertise first-class lazy pull support: {output}"
    );
}

#[test]
fn test_cli_clone_local_lazy_is_rejected() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();
    let clone_dir = target.path().join("clone");

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("hello.txt"), "from source").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();

    let source_path = source.path().to_string_lossy().to_string();
    let clone_path = clone_dir.to_string_lossy().to_string();
    let err = heddle(&["clone", &source_path, &clone_path, "--lazy"], None).unwrap_err();
    assert!(
        err.contains("lazy clone is only supported for hosted/network remotes"),
        "local lazy clone should fail with a clear message: {err}"
    );
}

#[test]
fn test_cli_clone_git_overlay_depth_is_rejected_for_local_path() {
    // Issue 49: --depth on a direct-filesystem Git-overlay clone has no
    // wire to negotiate `deepen N` over. Reject explicitly.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    gix::init_bare(&origin).expect("init bare git origin");

    let err = heddle(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
            "--depth",
            "1",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--depth") && err.contains("file://"),
        "depth-on-local-path should redirect to file:// URL form: {err}"
    );
}

#[test]
fn test_cli_clone_git_overlay_lazy_is_rejected_for_local_path() {
    // Issue 49: --lazy (the filter:blob:none synonym) on a
    // direct-filesystem Git-overlay clone is also redirected to the
    // file:// form so the wire path is engaged.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    gix::init_bare(&origin).expect("init bare git origin");

    let err = heddle(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
            "--lazy",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--lazy") && err.contains("file://"),
        "lazy-on-local-path should redirect to file:// URL form: {err}"
    );
}

#[test]
fn test_cli_clone_git_overlay_filter_is_rejected_for_local_path() {
    // `--filter` on the Git-overlay path is only wired through when
    // the source is a `file://` (or other URL) remote — the
    // direct-filesystem variant (`copy_local_repo_to_bare`) has no
    // wire to negotiate filter capability over, so we still reject
    // it explicitly. Issue #49 lifted the URL-side guard but kept
    // this one with a clearer redirection message.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    gix::init_bare(&origin).expect("init bare git origin");

    let err = heddle(
        &[
            "clone",
            origin.to_str().expect("origin path utf8"),
            work.to_str().expect("work path utf8"),
            "--filter",
            "blob:none",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--filter") && err.contains("file://"),
        "filter-on-local-path should redirect to file:// URL form: {err}"
    );
}

#[test]
fn test_cli_pull_local_lazy_is_rejected() {
    let source = TempDir::new().unwrap();
    let target = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    std::fs::write(source.path().join("hello.txt"), "from source").unwrap();
    heddle(&["capture", "-m", "Source state"], Some(source.path())).unwrap();
    heddle(&["init"], Some(target.path())).unwrap();

    let source_path = source.path().to_string_lossy().to_string();
    let err = heddle(&["pull", &source_path, "--lazy"], Some(target.path())).unwrap_err();
    assert!(
        err.contains("lazy pull is only supported for hosted/network remotes"),
        "local lazy pull should fail with a clear message: {err}"
    );
}

#[test]
fn test_cli_fetch_requires_remote_without_all() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let result = heddle(&["fetch"], Some(temp.path()));
    assert!(result.is_err(), "fetch without remote should fail");
    assert!(
        result.err().unwrap().contains("remote name required"),
        "fetch should explain missing remote"
    );
}

#[test]
fn test_cli_fetch_local_creates_remote_thread_and_marker() {
    let remote = TempDir::new().unwrap();
    let local = TempDir::new().unwrap();

    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(remote.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();
    heddle(&["marker", "create", "v1.0"], Some(remote.path())).unwrap();

    heddle(&["init"], Some(local.path())).unwrap();
    let remote_path = remote.path().to_string_lossy().to_string();
    heddle(
        &["remote", "add", "origin", &remote_path],
        Some(local.path()),
    )
    .unwrap();

    assert!(heddle(&["fetch", "origin"], Some(local.path())).is_ok());

    let repo = Repository::open(local.path()).unwrap();
    assert!(
        repo.refs()
            .get_remote_thread("origin", "main")
            .unwrap()
            .is_some()
    );
    assert!(repo.refs().get_marker("v1.0").unwrap().is_some());
}

#[test]
fn test_cli_fetch_all_uses_discovered_remotes() {
    let remote = TempDir::new().unwrap();
    let local = TempDir::new().unwrap();

    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(remote.path().join("file.txt"), "content").unwrap();
    heddle(&["capture", "-m", "Initial"], Some(remote.path())).unwrap();

    heddle(&["init"], Some(local.path())).unwrap();
    let remote_path = remote.path().to_string_lossy().to_string();
    heddle(
        &["remote", "add", "origin", &remote_path],
        Some(local.path()),
    )
    .unwrap();
    heddle(&["fetch", "origin"], Some(local.path())).unwrap();

    let output = heddle(&["fetch", "--all"], Some(local.path())).unwrap();
    assert!(
        output.contains("Fetched") || output.contains("\"refs_fetched\""),
        "fetch --all should report summary"
    );
}

#[test]
fn test_cli_push_defaults_to_current_attached_thread() {
    let source = TempDir::new().unwrap();
    let remote = TempDir::new().unwrap();

    heddle(&["init"], Some(source.path())).unwrap();
    heddle(&["init"], Some(remote.path())).unwrap();
    std::fs::write(source.path().join("base.txt"), "base").unwrap();
    heddle(&["capture", "-m", "init"], Some(source.path())).unwrap();

    let started: Value = serde_json::from_str(
        &heddle(
            &[
                "--json",
                "start",
                "feature/push-default",
                "--workspace",
                "private",
            ],
            Some(source.path()),
        )
        .unwrap(),
    )
    .unwrap();
    let thread = std::path::PathBuf::from(started["execution_path"].as_str().unwrap());
    std::fs::write(thread.join("feature.txt"), "feature").unwrap();
    heddle(&["capture", "-m", "feature"], Some(&thread)).unwrap();

    let remote_path = remote.path().to_string_lossy().to_string();
    heddle(&["push", &remote_path], Some(&thread)).unwrap();

    let remote_repo = Repository::open(remote.path()).unwrap();
    assert!(
        remote_repo
            .refs()
            .get_thread("feature/push-default")
            .unwrap()
            .is_some(),
        "push without --thread should update the current attached thread"
    );
}
