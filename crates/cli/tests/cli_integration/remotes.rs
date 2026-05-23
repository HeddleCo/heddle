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

    heddle(
        &["remote", "add", "backup", "localhost:8422"],
        Some(temp.path()),
    )
    .unwrap();
    heddle(&["remote", "set-default", "backup"], Some(temp.path())).unwrap();
    let json = heddle(&["--output", "json", "remote", "list"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("remote list JSON should parse");
    let remotes = parsed["remotes"].as_array().unwrap();
    assert!(
        remotes
            .iter()
            .any(|remote| remote["name"] == "backup" && remote["is_default"] == true),
        "remote list should mark the configured default: {parsed}"
    );
    let json = heddle(
        &["--output", "json", "remote", "show", "backup"],
        Some(temp.path()),
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("remote show JSON should parse");
    assert_eq!(parsed["is_default"], true);

    let trust_json = heddle(&["--output", "json", "trust"], Some(temp.path())).unwrap();
    let trust: Value = serde_json::from_str(&trust_json).expect("trust JSON should parse");
    assert_eq!(
        trust["trust"]["default_remote"], "backup",
        "trust should report the configured default remote: {trust}"
    );

    heddle(&["remote", "remove", "backup"], Some(temp.path())).unwrap();
    let result = heddle(&["remote", "remove", "origin"], Some(temp.path()));
    assert!(result.is_ok(), "Remote remove failed: {:?}", result.err());
    assert!(
        result.as_ref().unwrap().contains("removed remote origin"),
        "Remote remove should confirm deletion: {:?}",
        result.as_ref().ok()
    );

    let result = heddle(&["--output", "text", "remote", "list"], Some(temp.path())).unwrap();
    assert!(
        result.contains("No remotes configured"),
        "empty remote list should advertise the empty state: {result}"
    );
    let json = heddle(&["--output", "json", "remote", "list"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&json).expect("empty remote list JSON should parse");
    assert_eq!(parsed["remotes"].as_array().unwrap().len(), 0);
}

#[test]
fn test_cli_remote_show_missing_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output = heddle_output(
        &["--output", "json", "remote", "show", "missing"],
        Some(temp.path()),
    )
    .expect("invoke missing remote show");
    assert!(!output.status.success(), "remote show missing should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode remote show refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "remote_not_found");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("Remote 'missing' not found")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "missing remote refusal should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle remote list")
                && hint.contains("heddle remote add <name> <url>")),
        "missing remote hint should name inspect and setup commands: {stderr}"
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
            "--output",
            "json",
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
    assert_eq!(
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "pull --json must emit exactly one JSON value: {output}"
    );
    let parsed: Value = serde_json::from_str(&output).expect("pull JSON should parse");
    assert_eq!(
        parsed["success"], true,
        "pull should report success: {parsed}"
    );
    assert_eq!(parsed["trust"]["status"], "clean");

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
    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            &source_path,
            &clone_path,
            "--lazy",
        ],
        None,
    )
    .expect("invoke local lazy clone");
    assert!(
        !output.status.success(),
        "local lazy clone should fail with a typed refusal"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode local lazy clone refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !clone_dir.exists(),
        "local lazy clone refusal must run before destination initialization"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("local lazy clone should emit JSON envelope");
    assert_eq!(envelope["kind"], "local_clone_option_unsupported");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("--lazy is only supported")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "local lazy clone should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("without lazy/filter")),
        "local lazy clone hint should name the safe retry: {stderr}"
    );
}

#[test]
fn test_cli_clone_missing_local_remote_uses_typed_advice() {
    let target = TempDir::new().unwrap();
    let missing_remote = target.path().join("missing-source");
    let clone_dir = target.path().join("clone");

    let remote_path = missing_remote.to_string_lossy().to_string();
    let clone_path = clone_dir.to_string_lossy().to_string();
    let output = heddle_output(
        &["--output", "json", "clone", &remote_path, &clone_path],
        None,
    )
    .expect("invoke missing local remote clone");
    assert!(
        !output.status.success(),
        "clone with missing local remote should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing local remote refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !clone_dir.exists(),
        "missing local remote refusal must not initialize the destination"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing local remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "clone_remote_not_found");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("Remote repository")
                && error.contains("does not exist")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "missing local remote should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("retry `heddle clone`")),
        "missing local remote hint should name the safe retry: {stderr}"
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

#[test]
fn test_cli_clone_git_overlay_missing_requested_branch_uses_typed_advice() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source.git");
    let work = temp.path().join("work");

    let src = gix::init_bare(&source).expect("init bare source");
    let empty_tree: gix::ObjectId = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"
        .parse()
        .expect("parse empty tree oid");
    let sig = gix::actor::Signature {
        name: "Heddle Test".into(),
        email: "heddle@test".into(),
        time: gix::date::Time {
            seconds: 0,
            offset: 0,
        },
    };
    let mut committer_buf = gix::date::parse::TimeBuf::default();
    let mut author_buf = gix::date::parse::TimeBuf::default();
    let main_tip = src
        .new_commit_as(
            sig.to_ref(&mut committer_buf),
            sig.to_ref(&mut author_buf),
            "seed main",
            empty_tree,
            Vec::<gix::ObjectId>::new(),
        )
        .expect("commit succeeds")
        .id;
    src.reference(
        "refs/heads/main",
        main_tip,
        gix::refs::transaction::PreviousValue::Any,
        "test: seed main",
    )
    .expect("set main ref");
    std::fs::write(source.join("HEAD"), b"ref: refs/heads/main\n").unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "clone",
            source.to_str().expect("source path utf8"),
            work.to_str().expect("work path utf8"),
            "--thread",
            "missing",
        ],
        None,
    )
    .expect("invoke missing git-overlay branch clone");
    assert!(
        !output.status.success(),
        "clone with missing requested Git branch should fail"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode missing Git branch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        work.exists(),
        "Git-overlay import failure happens after clone preflight and should preserve the partial clone"
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing Git branch should emit JSON envelope");
    assert_eq!(envelope["kind"], "git_overlay_clone_import_failed");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("requested ref(s) not found")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "missing Git branch should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("existing commit-pointing branch")),
        "missing Git branch hint should name the safe retry: {stderr}"
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
    // Use an explicit signature via `new_commit_as` rather than
    // `Repository::commit`. The latter reads `user.name`/`user.email`
    // from git config, which CI runners don't set — leading to
    // `AuthorMissing` errors. The clone path under test doesn't care
    // who authored these seed commits.
    let sig = gix::actor::Signature {
        name: "Heddle Test".into(),
        email: "heddle@test".into(),
        time: gix::date::Time {
            seconds: 0,
            offset: 0,
        },
    };
    let commit_as = |message: &str, parents: Vec<gix::ObjectId>| -> gix::ObjectId {
        let mut committer_buf = gix::date::parse::TimeBuf::default();
        let mut author_buf = gix::date::parse::TimeBuf::default();
        src.new_commit_as(
            sig.to_ref(&mut committer_buf),
            sig.to_ref(&mut author_buf),
            message,
            empty_tree,
            parents,
        )
        .expect("commit succeeds")
        .id
    };
    let trunk_first = commit_as("seed trunk", vec![]);
    let trunk_tip = commit_as("advance trunk", vec![trunk_first]);
    let abc_feature = commit_as("seed abc-feature", vec![]);
    src.reference(
        "refs/heads/trunk",
        trunk_tip,
        gix::refs::transaction::PreviousValue::Any,
        "test: seed trunk",
    )
    .expect("set trunk ref");
    src.reference(
        "refs/heads/abc-feature",
        abc_feature,
        gix::refs::transaction::PreviousValue::Any,
        "test: seed abc-feature",
    )
    .expect("set abc-feature ref");
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

    let git_status = Command::new("git")
        .args(["-C", work_arg, "status", "--short"])
        .output()
        .expect("git status");
    assert!(
        git_status.status.success(),
        "git status should succeed after clone: {}",
        String::from_utf8_lossy(&git_status.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_status.stdout),
        "",
        "git-overlay clone should leave a Git-clean checkout"
    );

    let git_branch = Command::new("git")
        .args(["-C", work_arg, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .expect("git branch");
    assert!(
        git_branch.status.success(),
        "git branch should succeed after clone: {}",
        String::from_utf8_lossy(&git_branch.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&git_branch.stdout).trim(),
        "trunk",
        "Git HEAD should match the active Heddle thread"
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
    let output = heddle_output(
        &["--output", "json", "pull", &source_path, "--lazy"],
        Some(target.path()),
    )
    .expect("invoke local lazy pull");
    assert!(
        !output.status.success(),
        "local lazy pull should fail with a typed refusal"
    );
    assert!(
        output.stdout.is_empty(),
        "JSON-mode lazy pull refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("local lazy pull should emit JSON envelope");
    assert_eq!(envelope["kind"], "local_lazy_pull_unsupported");
    assert!(
        envelope["error"].as_str().is_some_and(|error| error
            .contains("lazy materialization requires a hosted or network remote")
            && error.contains("Unsafe:")
            && error.contains("Would change:")
            && error.contains("Preserved:")
            && error.contains("Primary recovery:")),
        "local lazy pull should include typed recovery detail: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("without `--lazy`")),
        "local lazy pull hint should name the safe retry: {stderr}"
    );
}

#[test]
fn test_cli_fetch_requires_remote_without_all() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();

    let output =
        heddle_output(&["--output", "json", "fetch"], Some(temp.path())).expect("invoke fetch");
    assert!(!output.status.success(), "fetch without remote should fail");
    assert!(
        output.stdout.is_empty(),
        "JSON-mode fetch refusal must keep stdout quiet: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = std::str::from_utf8(&output.stderr).unwrap();
    let envelope: Value =
        serde_json::from_str(stderr).expect("missing remote should emit JSON envelope");
    assert_eq!(envelope["kind"], "remote_name_required");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("remote name required")
                && error.contains("Unsafe:")
                && error.contains("Would change:")
                && error.contains("Preserved:")
                && error.contains("Primary recovery:")),
        "fetch should explain the typed missing-remote refusal: {stderr}"
    );
    assert!(
        envelope["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("heddle fetch <remote>")
                && hint.contains("heddle fetch --all")),
        "fetch hint should name both valid recovery shapes: {stderr}"
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
    let output = heddle(&["--output", "json", "push", &remote_path], Some(&thread)).unwrap();
    assert_eq!(
        output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "push --json must emit exactly one JSON value: {output}"
    );
    let parsed: Value = serde_json::from_str(&output).expect("push JSON should parse");
    assert_eq!(
        parsed["success"], true,
        "push should report success: {parsed}"
    );
    assert_eq!(parsed["trust"]["status"], "clean");

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
