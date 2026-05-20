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
fn test_cli_clone_git_overlay_depth_is_rejected() {
    // Issue 49 / 20b: `--depth` is wired through to gix at the wire
    // layer (`clone_url_to_bare` honours it), but the import step
    // (`import_all` ancestry walk) still requires every parent commit
    // locally. Until the importer tolerates missing parents, the
    // user-facing flag is rejected up-front so we never leave a
    // half-built clone behind.
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
        err.contains("--depth") && err.contains("not yet supported"),
        "depth must be rejected with 'not yet supported': {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
    );
}

#[test]
fn test_cli_clone_git_overlay_lazy_is_rejected() {
    // Issue 49 / 20b: same shape as --depth — `--lazy` (the
    // `--filter blob:none` synonym) gets rejected up-front because the
    // import step requires all blobs locally.
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
        err.contains("--lazy") && err.contains("not yet supported"),
        "lazy must be rejected with 'not yet supported': {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
    );
}

/// heddle#141 + heddle#142 regression: cloning a git repo whose `HEAD`
/// points at a branch that is not alphabetically first must (1) land
/// the user on the remote's actual default branch and (2) leave
/// `heddle log` walking the imported history, not just a freshly
/// minted bootstrap state.
///
/// We exercise the local-overlay path (`copy_local_repo_to_bare`)
/// because it's hermetic. The URL-overlay path has its own unit-level
/// regression in `bridge::git_core::tests` that verifies
/// `clone_url_to_bare` mirrors the remote symref into `.git/HEAD`,
/// which is what feeds the same `select_clone_thread` selection
/// logic this test pins.
#[test]
fn test_cli_clone_git_overlay_lands_on_remote_default_branch_and_log_walks_history() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");

    // Build a bare git source with `trunk` (the default, with two
    // commits so we can confirm log walks history) and
    // `abc-feature` (alphabetically first — the trap heddle#141 used
    // to fall into). Branch names deliberately avoid `main`/`master`
    // so neither gix's `init.defaultBranch` nor the previous
    // fallback could land here by accident.
    let src = gix::init_bare(&source).expect("init bare source");
    let empty_tree: gix::ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        .parse()
        .expect("parse empty tree oid");
    let trunk_first = src
        .commit(
            "refs/heads/trunk",
            "seed trunk",
            empty_tree,
            gix::commit::NO_PARENT_IDS,
        )
        .expect("commit trunk seed");
    src.commit(
        "refs/heads/trunk",
        "advance trunk",
        empty_tree,
        [trunk_first],
    )
    .expect("commit trunk tip");
    src.commit(
        "refs/heads/abc-feature",
        "seed abc-feature",
        empty_tree,
        gix::commit::NO_PARENT_IDS,
    )
    .expect("commit abc-feature");
    std::fs::write(source.join("HEAD"), b"ref: refs/heads/trunk\n").unwrap();

    let source_arg = source.to_str().expect("source path utf8");
    let work_arg = work.to_str().expect("work path utf8");
    let output = heddle(&["clone", source_arg, work_arg], None).expect("clone succeeds");
    assert!(
        output.contains("trunk"),
        "clone output should advertise the chosen branch (trunk): {output}"
    );

    // heddle#141: HEAD should land on `trunk`, not the
    // alphabetically-first `abc-feature`.
    let heddle_head =
        std::fs::read_to_string(work.join(".heddle").join("HEAD")).expect("read heddle HEAD");
    assert_eq!(
        heddle_head.trim(),
        "ref: trunk",
        "heddle HEAD must attach to the remote's default branch (trunk), \
         not the alphabetically-first imported branch (abc-feature) — \
         see heddle#141. Got: {heddle_head:?}"
    );

    // heddle#142: log must walk the imported history, not surface a
    // freshly-minted bootstrap state. Two trunk commits → two real
    // states (the synthetic `heddle init` root is filtered).
    let log_json = heddle(&["log", "--output", "json"], Some(&work)).expect("log succeeds");
    let parsed: serde_json::Value = serde_json::from_str(&log_json).expect("log json parses");
    let states = parsed
        .get("states")
        .and_then(|s| s.as_array())
        .expect("log output has a states array");
    assert!(
        states.len() >= 2,
        "heddle log should walk the imported trunk history (>=2 states), \
         not just a fresh bootstrap snapshot — see heddle#142. \
         States: {states:#?}"
    );
    let bootstrap_only = states.len() == 1
        && states[0]
            .get("intent")
            .and_then(|v| v.as_str())
            .is_some_and(|intent| intent.contains("Bootstrap git-overlay"));
    assert!(
        !bootstrap_only,
        "heddle log surfaced only the synthetic bootstrap state — \
         this is the heddle#142 failure mode. States: {states:#?}"
    );
}

#[test]
fn test_cli_clone_git_overlay_filter_is_rejected() {
    // Issue 49 / 20b: same shape as --depth / --lazy — `--filter` is
    // rejected up-front. The wire-layer plumbing in `clone_url_to_bare`
    // is real prep for 20c; the user-facing flag flip waits on
    // import-side support.
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
        err.contains("--filter") && err.contains("not yet supported"),
        "filter must be rejected with 'not yet supported': {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
    );
}

#[test]
fn test_cli_clone_git_overlay_file_url_rejects_unsupported_flags() {
    // Issue 49 / 20b round-2 P1: previously the local-path rejection
    // told users to "use a file:// URL instead" — but `file://` parses
    // as `RemoteTarget::Local` and routes through the same
    // `clone_git_overlay_path`, hitting the same rejection. Confirm
    // the file:// scheme path is rejected with the same shape (no
    // dead-end loop) and leaves no partial directory behind.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    gix::init_bare(&origin).expect("init bare git origin");

    let file_url = format!("file://{}", origin.display());
    let err = heddle(
        &[
            "clone",
            &file_url,
            work.to_str().expect("work path utf8"),
            "--filter",
            "blob:none",
        ],
        None,
    )
    .unwrap_err();
    assert!(
        err.contains("--filter") && err.contains("not yet supported"),
        "file:// + --filter must reject with the same 'not yet supported' shape: {err}"
    );
    assert!(
        !work.exists(),
        "rejection must run before any filesystem work: {} should not exist",
        work.display()
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
    assert!(repo
        .refs()
        .get_remote_thread("origin", "main")
        .unwrap()
        .is_some());
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
                "auto",
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
