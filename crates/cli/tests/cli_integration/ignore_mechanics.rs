// SPDX-License-Identifier: Apache-2.0
use super::*;

fn captured_path_exists(root: &Path, path: &str) -> bool {
    let repo = Repository::open(root).expect("repository should open");
    let state = repo
        .current_state()
        .expect("HEAD should resolve")
        .expect("capture should create a state");
    state_path_exists(&repo, &state, path)
}

fn state_path_exists(repo: &Repository, state: &objects::object::State, path: &str) -> bool {
    objects::object::resolve_tree_path(
        repo.store(),
        &state.tree,
        Path::new(path),
        objects::object::LeafPolicy::Entry,
    )
    .expect("captured tree path should resolve")
    .is_some()
}

#[test]
fn native_capture_honours_root_gitignore_without_git_metadata() {
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join(".gitignore"), "git-only.log\n").unwrap();
    std::fs::write(temp.path().join("git-only.log"), "ignored\n").unwrap();
    std::fs::write(temp.path().join("kept.txt"), "captured\n").unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["capture", "-m", "honour root gitignore"],
        Some(temp.path()),
    )
    .unwrap();

    assert!(!temp.path().join(".git").exists());
    assert!(!captured_path_exists(temp.path(), "git-only.log"));
    assert!(captured_path_exists(temp.path(), "kept.txt"));
}

#[test]
fn root_ignore_files_form_one_ordered_stream_and_nested_files_are_not_read() {
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join(".gitignore"),
        "git-only.log\nnegated.log\n!reverse.log\nrepeat.log\n!repeat.log\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join(".heddleignore"),
        "heddle-only.tmp\n!negated.log\nreverse.log\nrepeat.log\n",
    )
    .unwrap();
    for path in [
        "git-only.log",
        "heddle-only.tmp",
        "negated.log",
        "reverse.log",
        "repeat.log",
    ] {
        std::fs::write(temp.path().join(path), path).unwrap();
    }
    std::fs::create_dir(temp.path().join("nested")).unwrap();
    std::fs::write(temp.path().join("nested/.gitignore"), "nested-only.txt\n").unwrap();
    std::fs::write(temp.path().join("nested/nested-only.txt"), "captured\n").unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["capture", "-m", "merge root ignore files"],
        Some(temp.path()),
    )
    .unwrap();

    assert!(!captured_path_exists(temp.path(), "git-only.log"));
    assert!(!captured_path_exists(temp.path(), "heddle-only.tmp"));
    assert!(captured_path_exists(temp.path(), "negated.log"));
    assert!(!captured_path_exists(temp.path(), "reverse.log"));
    assert!(!captured_path_exists(temp.path(), "repeat.log"));
    assert!(captured_path_exists(temp.path(), "nested/nested-only.txt"));
}

#[test]
fn native_capture_with_neither_ignore_file_keeps_requested_paths() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join("target")).unwrap();
    std::fs::write(temp.path().join("target/output.bin"), "captured\n").unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(
        &["capture", "-m", "capture without ignore policy"],
        Some(temp.path()),
    )
    .unwrap();

    assert!(captured_path_exists(temp.path(), "target/output.bin"));
}

#[test]
fn adding_an_ignore_rule_cleans_the_next_state_but_not_existing_history() {
    let temp = TempDir::new().unwrap();
    std::fs::create_dir(temp.path().join("target")).unwrap();
    std::fs::write(temp.path().join("target/output.bin"), "pollution\n").unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "capture pollution"], Some(temp.path())).unwrap();

    let repo = Repository::open(temp.path()).unwrap();
    let polluted = repo.current_state().unwrap().unwrap();
    assert!(state_path_exists(&repo, &polluted, "target/output.bin"));
    drop(repo);

    std::fs::write(temp.path().join(".gitignore"), "target/\n").unwrap();
    heddle(
        &["capture", "-m", "remove ignored artifacts"],
        Some(temp.path()),
    )
    .unwrap();

    let repo = Repository::open(temp.path()).unwrap();
    let cleaned = repo.current_state().unwrap().unwrap();
    assert!(!state_path_exists(&repo, &cleaned, "target/output.bin"));
    let historical = repo.store().get_state(&polluted.state_id).unwrap().unwrap();
    assert!(state_path_exists(&repo, &historical, "target/output.bin"));
}

#[test]
fn bulk_capture_warns_but_ordinary_capture_does_not() {
    let ordinary = TempDir::new().unwrap();
    heddle(&["init"], Some(ordinary.path())).unwrap();
    for index in 0..3 {
        std::fs::write(
            ordinary.path().join(format!("file-{index}.txt")),
            "ordinary\n",
        )
        .unwrap();
    }
    let ordinary_output = heddle(
        &["capture", "-m", "ordinary capture"],
        Some(ordinary.path()),
    )
    .unwrap();
    assert!(!ordinary_output.contains("Warning: captured"));

    let bulk = TempDir::new().unwrap();
    heddle(&["init"], Some(bulk.path())).unwrap();
    for index in 0..500 {
        std::fs::write(bulk.path().join(format!("artifact-{index}.bin")), "bulk\n").unwrap();
    }
    let bulk_output = heddle(&["capture", "-m", "bulk capture"], Some(bulk.path())).unwrap();
    assert!(
        bulk_output.contains("Warning: captured 500 paths in one operation"),
        "bulk capture should warn: {bulk_output}"
    );
    assert!(bulk_output.contains(".gitignore"));
    assert!(bulk_output.contains(".heddleignore"));
}

#[test]
fn heddleignore_help_topic_prints_the_documented_contract() {
    let help = heddle_help(&["help", "heddleignore"]);
    assert!(!help.contains("no topic or command"));
    assert!(help.contains("# `.heddleignore`"));
    assert!(help.contains("one ordered rule stream"));
    assert!(help.contains("500 or more paths"));
    assert!(help.contains("[worktree] ignore"));
    assert!(help.contains("no `--path` filter") || help.contains("no `--path`"));
}

#[test]
fn ignore_help_alias_renders_the_same_topic() {
    let help = heddle_help(&["help", "ignore"]);
    assert!(!help.contains("no topic or command"));
    assert!(help.contains("# `.heddleignore`"));
    assert!(help.contains(".gitignore"));
    assert!(help.contains("[worktree] ignore"));
}

#[test]
fn status_and_capture_omit_python_build_junk_listed_in_gitignore() {
    // Evaluator repro for heddle#1155: pytest leaves `__pycache__` / `.pyc`
    // next to source; with those patterns in root `.gitignore`, status must
    // not list them and capture must not record them.
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join(".gitignore"),
        "__pycache__/\n*.pyc\n.pytest_cache/\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("app.py"), "print('hi')\n").unwrap();
    std::fs::write(temp.path().join("kept.txt"), "real\n").unwrap();
    std::fs::create_dir_all(temp.path().join("__pycache__")).unwrap();
    std::fs::create_dir_all(temp.path().join("src/__pycache__")).unwrap();
    std::fs::create_dir_all(temp.path().join(".pytest_cache/v")).unwrap();
    std::fs::write(
        temp.path().join("__pycache__/app.cpython-312.pyc"),
        "binary",
    )
    .unwrap();
    std::fs::write(
        temp.path().join("src/__pycache__/mod.cpython-312.pyc"),
        "binary",
    )
    .unwrap();
    std::fs::write(temp.path().join("src/app.pyc"), "binary").unwrap();
    std::fs::write(temp.path().join(".pytest_cache/v/cache"), "cache").unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    let status = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(
        !status.contains("__pycache__")
            && !status.contains(".pyc")
            && !status.contains(".pytest_cache"),
        "status must not list gitignored build junk: {status}"
    );
    assert!(
        status.contains("kept.txt") && status.contains("app.py"),
        "status must still list real worktree files: {status}"
    );

    heddle(
        &["capture", "-m", "seed without build junk"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(!captured_path_exists(
        temp.path(),
        "__pycache__/app.cpython-312.pyc"
    ));
    assert!(!captured_path_exists(
        temp.path(),
        "src/__pycache__/mod.cpython-312.pyc"
    ));
    assert!(!captured_path_exists(temp.path(), "src/app.pyc"));
    assert!(!captured_path_exists(temp.path(), ".pytest_cache/v/cache"));
    assert!(captured_path_exists(temp.path(), "kept.txt"));
    assert!(captured_path_exists(temp.path(), "app.py"));
}

#[test]
fn thread_checkout_status_and_capture_honour_root_gitignore() {
    // heddle#1155: isolated thread checkouts must use the same ignore
    // stream as the origin root, including root `.gitignore`.
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin");
    let thread = temp.path().join("thread");
    std::fs::create_dir(&origin).unwrap();
    std::fs::write(
        origin.join(".gitignore"),
        "__pycache__/\n*.pyc\n.pytest_cache/\n",
    )
    .unwrap();
    std::fs::write(origin.join("app.py"), "print('hi')\n").unwrap();
    heddle(&["init"], Some(&origin)).unwrap();
    heddle(&["capture", "-m", "seed"], Some(&origin)).unwrap();
    heddle(
        &[
            "start",
            "feature/pytest",
            "--path",
            thread.to_str().expect("utf-8 thread path"),
        ],
        Some(&origin),
    )
    .unwrap();

    std::fs::create_dir_all(thread.join("__pycache__")).unwrap();
    std::fs::create_dir_all(thread.join("src/__pycache__")).unwrap();
    std::fs::create_dir_all(thread.join(".pytest_cache/v")).unwrap();
    std::fs::write(thread.join("__pycache__/app.cpython-312.pyc"), "binary").unwrap();
    std::fs::write(thread.join("src/__pycache__/mod.cpython-312.pyc"), "binary").unwrap();
    std::fs::write(thread.join(".pytest_cache/v/cache"), "cache").unwrap();
    std::fs::write(thread.join("feature.txt"), "new feature\n").unwrap();

    let status = heddle(&["status"], Some(&thread)).unwrap();
    assert!(
        !status.contains("__pycache__")
            && !status.contains(".pyc")
            && !status.contains(".pytest_cache"),
        "thread status must not list gitignored build junk: {status}"
    );
    assert!(
        status.contains("feature.txt"),
        "thread status must list the real unignored change: {status}"
    );

    heddle(&["capture", "-m", "after pytest junk"], Some(&thread)).unwrap();
    assert!(!captured_path_exists(
        &thread,
        "__pycache__/app.cpython-312.pyc"
    ));
    assert!(!captured_path_exists(
        &thread,
        "src/__pycache__/mod.cpython-312.pyc"
    ));
    assert!(!captured_path_exists(&thread, ".pytest_cache/v/cache"));
    assert!(captured_path_exists(&thread, "feature.txt"));
    assert!(captured_path_exists(&thread, "app.py"));
}

#[test]
fn removing_gitignore_rule_makes_junk_reappear_in_status_and_capture() {
    // Guard is load-bearing: without the ignore rule the same paths return.
    let temp = TempDir::new().unwrap();
    std::fs::write(temp.path().join(".gitignore"), "__pycache__/\n").unwrap();
    std::fs::write(temp.path().join("kept.txt"), "real\n").unwrap();
    std::fs::create_dir(temp.path().join("__pycache__")).unwrap();
    std::fs::write(temp.path().join("__pycache__/app.pyc"), "binary").unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    heddle(&["capture", "-m", "ignored junk"], Some(temp.path())).unwrap();
    assert!(!captured_path_exists(temp.path(), "__pycache__/app.pyc"));

    std::fs::write(temp.path().join(".gitignore"), "").unwrap();
    let status = heddle(&["status"], Some(temp.path())).unwrap();
    assert!(
        status.contains("__pycache__") || status.contains("app.pyc"),
        "clearing the ignore rule must surface the junk again: {status}"
    );
    heddle(
        &["capture", "-m", "junk no longer ignored"],
        Some(temp.path()),
    )
    .unwrap();
    assert!(captured_path_exists(temp.path(), "__pycache__/app.pyc"));
    assert!(captured_path_exists(temp.path(), "kept.txt"));
}

#[test]
fn native_capture_cannot_unignore_heddle_identity_via_gitignore() {
    // heddle#1413: last-match-wins used to let `!.heddle/` pull
    // identity.toml into captured history. Root `.heddle/` is reserved
    // after user rules; nested fixture `.heddle/` stays ordinary content.
    let temp = TempDir::new().unwrap();
    std::fs::write(
        temp.path().join(".gitignore"),
        "!.heddle/\n!.heddle/**\n!.heddle/identity.toml\n",
    )
    .unwrap();
    std::fs::write(
        temp.path().join(".heddleignore"),
        "!.heddle/\n!.heddle/identity.toml\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("kept.txt"), "captured\n").unwrap();

    heddle(&["init"], Some(temp.path())).unwrap();
    std::fs::write(
        temp.path().join(".heddle").join("identity.toml"),
        "secret-key-material\n",
    )
    .unwrap();
    std::fs::create_dir_all(temp.path().join("examples/calculator/.heddle")).unwrap();
    std::fs::write(
        temp.path()
            .join("examples/calculator/.heddle/identity.toml"),
        "fixture\n",
    )
    .unwrap();

    let status = heddle(&["status", "--output", "json-compact"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).expect("status json");
    let paths: Vec<&str> = parsed["changed_paths"]
        .as_array()
        .unwrap_or_else(|| panic!("status must carry changed_paths: {status}"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        paths.iter().any(|path| *path == "kept.txt"),
        "status must still list real worktree files: {paths:?}"
    );
    assert!(
        paths.iter().all(|path| !path.starts_with(".heddle")),
        "status must not list reserved root .heddle paths: {paths:?}"
    );
    assert!(
        paths
            .iter()
            .any(|path| *path == "examples/calculator/.heddle/identity.toml"),
        "status must still list nested fixture .heddle: {paths:?}"
    );

    heddle(
        &["capture", "-m", "must not capture reserved identity"],
        Some(temp.path()),
    )
    .unwrap();

    assert!(!captured_path_exists(temp.path(), ".heddle/identity.toml"));
    assert!(!captured_path_exists(temp.path(), ".heddle"));
    assert!(captured_path_exists(temp.path(), "kept.txt"));
    assert!(captured_path_exists(
        temp.path(),
        "examples/calculator/.heddle/identity.toml"
    ));
}

fn init_text_points_at_ignore_help_when_heddleignore_not_installed() {
    let temp = TempDir::new().unwrap();
    let out = heddle(&["init"], Some(temp.path())).unwrap();
    assert!(
        out.contains("heddle help ignore") || out.contains("`heddle help ignore`"),
        "init must point at ignore docs when it does not install .heddleignore: {out}"
    );
    assert!(
        out.contains(".heddleignore") || out.contains("Ignore:"),
        "init must name the ignore surface: {out}"
    );
}
