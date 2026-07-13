// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use super::*;

fn start_materialized_thread(root: &Path, name: &str) -> PathBuf {
    let path = root.join("threads").join(name);
    let output = heddle(
        &[
            "--output",
            "json",
            "start",
            name,
            "--workspace",
            "materialized",
            "--path",
            path.to_str().unwrap(),
        ],
        Some(root),
    )
    .unwrap_or_else(|error| panic!("start {name} failed: {error}"));
    let started: Value = serde_json::from_str(&output).expect("start output should be JSON");
    let execution_path = PathBuf::from(
        started["execution_path"]
            .as_str()
            .expect("start must report its execution path"),
    );
    assert_eq!(
        execution_path.canonicalize().unwrap(),
        path.canonicalize().unwrap(),
        "start must use the requested materialized checkout: {started}"
    );

    let shown = heddle(&["--output", "json", "thread", "show", name], Some(root))
        .unwrap_or_else(|error| panic!("show {name} failed: {error}"));
    let shown: Value = serde_json::from_str(&shown).expect("thread show output should be JSON");
    assert_eq!(
        shown["target_thread"], "main",
        "materialized sibling must target main: {shown}"
    );
    assert!(execution_path.is_dir(), "thread checkout must exist");
    execution_path
}

fn capture(path: &Path, message: &str) {
    heddle(&["capture", "-m", message], Some(path))
        .unwrap_or_else(|error| panic!("capture {message:?} failed: {error}"));
}

fn ready_and_land(root: &Path, thread: &str) {
    heddle(
        &["--output", "json", "ready", "--thread", thread],
        Some(root),
    )
    .unwrap_or_else(|error| panic!("ready {thread} failed: {error}"));
    let output = heddle(
        &["--output", "json", "land", "--thread", thread],
        Some(root),
    )
    .unwrap_or_else(|error| panic!("land {thread} failed: {error}"));
    let landed: Value = serde_json::from_str(&output).expect("land output should be JSON");
    assert_eq!(landed["status"], "landed", "{landed}");
    assert_eq!(landed["integrated"], true, "{landed}");
}

fn refresh_failure(root: &Path, thread: &str) -> Value {
    let output = heddle_output(
        &["--output", "json", "thread", "refresh", thread],
        Some(root),
    )
    .unwrap_or_else(|error| panic!("refresh invocation failed: {error}"));
    assert!(
        !output.status.success(),
        "refresh {thread} should report a conflict"
    );
    assert!(
        output.stdout.is_empty(),
        "failed JSON refresh should not emit stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "refresh failure should be JSON ({error}): {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn assert_markers_at_column_zero(content: &str, context: &str) {
    for marker in ["<<<<<<<", "=======", ">>>>>>>"] {
        let marker_lines = content
            .lines()
            .filter(|line| line.contains(marker))
            .collect::<Vec<_>>();
        assert!(
            !marker_lines.is_empty(),
            "missing {marker:?} marker for {context}: {content}"
        );
        assert!(
            marker_lines.iter().all(|line| line.starts_with(marker)),
            "marker {marker:?} is not anchored at column zero for {context}: {content}"
        );
    }
}

#[test]
fn land_preserves_ignored_sibling_below_removed_tracked_directory() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join(".heddleignore"), "node_modules/\n").unwrap();
    fs::create_dir_all(temp.path().join("web")).unwrap();
    fs::write(temp.path().join("web/index.html"), "<html/>\n").unwrap();
    capture(temp.path(), "base web tree");

    let feature = start_materialized_thread(temp.path(), "drop-web");
    fs::remove_dir_all(feature.join("web")).unwrap();
    capture(&feature, "drop tracked web tree");

    let ignored = temp.path().join("web/node_modules/lodash/index.js");
    fs::create_dir_all(ignored.parent().unwrap()).unwrap();
    fs::write(&ignored, "ignored\n").unwrap();

    ready_and_land(temp.path(), "drop-web");

    assert_file_not_exists(
        temp.path().join("web/index.html"),
        "land must remove the tracked file",
    );
    assert_file_exists(&ignored, "land must preserve the ignored sibling");
}

#[test]
fn disjoint_siblings_refresh_then_land_without_losing_either_change() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "base.txt", "shared base\n");
    let alpha = start_materialized_thread(temp.path(), "alpha");
    let beta = start_materialized_thread(temp.path(), "beta");

    fs::write(alpha.join("alpha.txt"), "alpha\n").unwrap();
    capture(&alpha, "alpha change");
    fs::write(beta.join("beta.txt"), "beta\n").unwrap();
    capture(&beta, "beta change");

    ready_and_land(temp.path(), "alpha");
    heddle(
        &["--output", "json", "thread", "refresh", "beta"],
        Some(temp.path()),
    )
    .unwrap_or_else(|error| panic!("disjoint beta refresh failed: {error}"));

    assert_eq!(
        fs::read_to_string(beta.join("alpha.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(fs::read_to_string(beta.join("beta.txt")).unwrap(), "beta\n");
    ready_and_land(temp.path(), "beta");
    assert_eq!(
        fs::read_to_string(temp.path().join("alpha.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        fs::read_to_string(temp.path().join("beta.txt")).unwrap(),
        "beta\n"
    );
}

#[test]
fn refresh_conflict_names_path_and_persists_resolve_state() {
    let temp = TempDir::new().unwrap();
    setup_repo_with_file(&temp, "contested.txt", "base\n");
    let alpha = start_materialized_thread(temp.path(), "alpha");
    let beta = start_materialized_thread(temp.path(), "beta");

    fs::write(alpha.join("contested.txt"), "alpha\n").unwrap();
    capture(&alpha, "alpha edit");
    fs::write(beta.join("contested.txt"), "beta\n").unwrap();
    capture(&beta, "beta edit");
    ready_and_land(temp.path(), "alpha");

    let envelope = refresh_failure(temp.path(), "beta");
    assert_eq!(envelope["kind"], "thread_refresh_conflicted", "{envelope}");
    assert!(
        envelope["error"]
            .as_str()
            .is_some_and(|error| error.contains("contested.txt")),
        "refresh refusal must name the conflicting path: {envelope}"
    );
    assert_json_recovery_advice_fields(&envelope, &envelope.to_string());

    let listed = heddle(
        &[
            "--repo",
            beta.to_str().unwrap(),
            "--output",
            "json",
            "resolve",
            "--list",
        ],
        Some(temp.path()),
    )
    .expect("persisted conflict should be listable from the beta checkout");
    let listed: Value = serde_json::from_str(&listed).expect("resolve list should be JSON");
    assert_eq!(listed["conflicts"], serde_json::json!(["contested.txt"]));
    assert_markers_at_column_zero(
        &fs::read_to_string(beta.join("contested.txt")).unwrap(),
        "persisted refresh conflict",
    );
}

#[test]
fn aborting_refresh_conflict_restores_topic_and_keeps_target_clean() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("contested.bin"), b"base\0").unwrap();
    capture(temp.path(), "base binary");
    let alpha = start_materialized_thread(temp.path(), "alpha");
    let beta = start_materialized_thread(temp.path(), "beta");

    fs::write(alpha.join("contested.bin"), b"alpha\0").unwrap();
    capture(&alpha, "alpha binary edit");
    fs::write(beta.join("contested.bin"), b"beta\0").unwrap();
    capture(&beta, "beta binary edit");
    ready_and_land(temp.path(), "alpha");

    let envelope = refresh_failure(temp.path(), "beta");
    assert_eq!(envelope["kind"], "thread_refresh_conflicted", "{envelope}");
    let abort = heddle(
        &[
            "--repo",
            beta.to_str().unwrap(),
            "--output",
            "json",
            "abort",
        ],
        Some(temp.path()),
    )
    .expect("abort should clear the beta conflict");
    let abort: Value = serde_json::from_str(&abort).expect("abort should emit JSON");
    assert_eq!(abort["status"], "aborted", "{abort}");
    assert_eq!(fs::read(beta.join("contested.bin")).unwrap(), b"beta\0");
    assert_eq!(
        fs::read(temp.path().join("contested.bin")).unwrap(),
        b"alpha\0"
    );

    let status = status_json(temp.path());
    assert_eq!(status["thread"], "main", "{status}");
    assert!(status["operation"].is_null(), "{status}");
    for kind in ["modified", "added", "deleted"] {
        assert_eq!(status["changes"][kind], serde_json::json!([]), "{status}");
    }
}

#[test]
fn refresh_conflict_markers_are_well_formed_for_every_newline_shape() {
    let cases = [
        ("ours-only-newline", "ours\n", "theirs"),
        ("theirs-only-newline", "ours", "theirs\n"),
        ("neither-newline", "ours", "theirs"),
        ("both-newline", "ours\n", "theirs\n"),
    ];

    for (label, main_content, topic_content) in cases {
        let temp = TempDir::new().unwrap();
        setup_repo_with_file(&temp, "file.txt", "base\n");
        let feature = start_materialized_thread(temp.path(), "feature");
        fs::write(feature.join("file.txt"), topic_content).unwrap();
        capture(&feature, "topic edit");
        fs::write(temp.path().join("file.txt"), main_content).unwrap();
        capture(temp.path(), "main edit");

        let envelope = refresh_failure(temp.path(), "feature");
        assert_eq!(envelope["kind"], "thread_refresh_conflicted", "{envelope}");
        let content = fs::read_to_string(feature.join("file.txt")).unwrap();
        assert_markers_at_column_zero(&content, label);
        assert!(
            !content.contains("ours=======") && !content.contains("theirs======="),
            "content must not be glued to a separator for {label}: {content}"
        );
    }
}

#[cfg(feature = "semantic")]
#[test]
fn refresh_routes_structural_reorder_through_semantic_merge() {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    let base = "fn a() { let x = 1; }\nfn b() { let x = 2; }\nfn c() { let x = 3; }\nfn d() { let x = 4; }\n";
    fs::write(temp.path().join("lib.rs"), base).unwrap();
    capture(temp.path(), "base functions");
    let feature = start_materialized_thread(temp.path(), "feature");

    fs::write(
        feature.join("lib.rs"),
        "fn d() { let x = 4; }\nfn c() { let x = 3; }\nfn b() { let x = 22; }\nfn a() { let x = 1; }\n",
    )
    .unwrap();
    capture(&feature, "reorder and edit b");
    fs::write(
        temp.path().join("lib.rs"),
        "fn a() { let x = 1; }\nfn b() { let x = 2; }\nfn c() { let x = 3; }\nfn d() { let x = 44; }\n",
    )
    .unwrap();
    capture(temp.path(), "edit d");

    heddle(
        &["--output", "json", "thread", "refresh", "feature"],
        Some(temp.path()),
    )
    .unwrap_or_else(|error| panic!("semantic refresh failed: {error}"));
    let merged = fs::read_to_string(feature.join("lib.rs")).unwrap();
    assert!(!merged.contains("<<<<<<<"), "{merged}");
    assert!(merged.contains("fn b() { let x = 22; }"), "{merged}");
    assert!(merged.contains("fn d() { let x = 44; }"), "{merged}");
}
