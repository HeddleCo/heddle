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
}
