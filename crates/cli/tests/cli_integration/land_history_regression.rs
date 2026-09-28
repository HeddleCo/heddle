// SPDX-License-Identifier: Apache-2.0
use std::path::PathBuf;

use serde_json::Value;

use super::{git_overlay_fixtures::GitOverlayFixture, *};

fn git_text(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .expect("run git");
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8(output.stdout)
        .expect("utf8 git output")
        .trim()
        .to_string()
}

fn bytes_under(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn collect(root: &Path, path: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        for entry in std::fs::read_dir(path).expect("read snapshot directory") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                collect(root, &path, files);
            } else {
                files.push((
                    path.strip_prefix(root)
                        .expect("relative path")
                        .to_path_buf(),
                    std::fs::read(&path).expect("read snapshot file"),
                ));
            }
        }
    }
    let mut files = Vec::new();
    collect(root, root, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

fn r7_fanout() -> (GitOverlayFixture, PathBuf, PathBuf) {
    let fixture = GitOverlayFixture::imported_main();
    let root = fixture.path();
    std::fs::create_dir(root.join("src")).expect("create sources");
    for name in ["a", "b", "c"] {
        std::fs::write(
            root.join(format!("src/{name}.rs")),
            format!("pub fn {name}() -> u32 {{ 1 }}\n"),
        )
        .expect("seed source");
    }
    fixture.json(&["--output", "json", "capture", "-m", "seed sources"]);
    let started = fixture.json(&[
        "--output",
        "json",
        "agent",
        "fanout",
        "start",
        "--title",
        "T",
        "--lane",
        "lane/one=One",
        "--lane",
        "lane/two=Two",
    ]);
    let lanes = started["lanes"].as_array().expect("fanout lanes");
    let mut paths = Vec::new();
    for lane in lanes {
        let path = PathBuf::from(lane["path"].as_str().expect("lane path"));
        let lease = lane["lease_id"].as_str().expect("lease id");
        let token = lane["token"].as_str().expect("lease token");
        let released = heddle_output_env(
            &[
                "agent", "release", "--lease", lease, "--token", token, "--status", "complete",
            ],
            Some(&path),
            &[],
        )
        .expect("release lane lease");
        assert!(released.status.success(), "release: {released:?}");
        paths.push(path);
    }
    std::fs::write(root.join("README.md"), "base\nparent moves\n").expect("move parent");
    fixture.json(&["--output", "json", "capture", "-m", "parent moves"]);
    fixture.json(&["--output", "json", "thread", "refresh", "lane/two"]);
    std::fs::write(paths[1].join("src/c.rs"), "pub fn c() -> u32 { 2 }\n").expect("edit lane two");
    fixture.json_at(&paths[1], &["--output", "json", "capture", "-m", "two"]);
    std::fs::write(paths[0].join("src/a.rs"), "pub fn a() -> u32 { 2 }\n").expect("edit lane one");
    fixture.json_at(&paths[0], &["--output", "json", "capture", "-m", "one"]);
    (fixture, paths.remove(0), paths.remove(0))
}

fn land_two(fixture: &GitOverlayFixture) {
    let output = fixture.json(&["--output", "json", "land", "--thread", "lane/two"]);
    assert_eq!(output["status"], "landed", "{output}");
}

#[test]
fn r7_stale_sibling_land_preserves_landed_git_history() {
    let (fixture, _, _) = r7_fanout();
    land_two(&fixture);
    let before = git_text(fixture.path(), &["rev-parse", "HEAD"]);
    let land = heddle_output_env(
        &["--output", "json", "land", "--thread", "lane/one"],
        Some(fixture.path()),
        &[],
    )
    .expect("land stale sibling");
    let after = git_text(fixture.path(), &["rev-parse", "HEAD"]);
    let ancestry = Command::new("git")
        .args(["merge-base", "--is-ancestor", &before, "HEAD"])
        .current_dir(fixture.path())
        .status()
        .expect("check main ancestry");
    assert!(
        ancestry.success(),
        "land rewrote main: {before} -> {after}; {land:?}"
    );
    if !land.status.success() {
        assert_eq!(
            after, before,
            "failed land must leave main unchanged: {land:?}"
        );
    }
}

#[test]
fn plain_land_after_parent_advance_preserves_git_history() {
    let fixture = GitOverlayFixture::imported_main();
    let checkout = fixture.path().with_file_name("plain-thread-checkout");
    fixture.json(&[
        "--output",
        "json",
        "start",
        "feature/plain",
        "--path",
        checkout.to_str().expect("checkout path"),
    ]);
    std::fs::write(checkout.join("feature.txt"), "first\n").expect("first edit");
    fixture.json_at(&checkout, &["--output", "json", "capture", "-m", "first"]);
    std::fs::write(checkout.join("feature.txt"), "first\nsecond\n").expect("second edit");
    fixture.json_at(&checkout, &["--output", "json", "capture", "-m", "second"]);
    std::fs::write(fixture.path().join("README.md"), "base\nparent moves\n").expect("parent edit");
    fixture.json(&["--output", "json", "capture", "-m", "parent moves"]);
    let before = git_text(fixture.path(), &["rev-parse", "HEAD"]);
    let land = heddle_output_env(
        &["--output", "json", "land", "--thread", "feature/plain"],
        Some(fixture.path()),
        &[],
    )
    .expect("land ordinary thread");
    let after = git_text(fixture.path(), &["rev-parse", "HEAD"]);
    let ancestry = Command::new("git")
        .args(["merge-base", "--is-ancestor", &before, "HEAD"])
        .current_dir(fixture.path())
        .status()
        .expect("check main ancestry");
    assert!(
        ancestry.success(),
        "ordinary land rewrote main: {before} -> {after}; {land:?}"
    );
    if !land.status.success() {
        assert_eq!(after, before, "failed ordinary land moved main: {land:?}");
    }
}

#[test]
fn verification_blocked_land_leaves_git_refs_and_oplog_byte_identical() {
    let (fixture, _, _) = r7_fanout();
    let repository = Repository::open(fixture.path()).expect("open target");
    let manager = repo::ThreadManager::new(repository.heddle_dir());
    let stale_sibling_state = manager
        .load_id_or_name("lane/one")
        .expect("load sibling")
        .expect("sibling exists")
        .current_state
        .expect("captured sibling state");
    land_two(&fixture);
    let repository = Repository::open(fixture.path()).expect("open target");
    let manager = repo::ThreadManager::new(repository.heddle_dir());
    let mut lane_two = manager
        .load_id_or_name("lane/two")
        .expect("load landed thread")
        .expect("landed thread exists");
    lane_two.current_state = Some(stale_sibling_state.clone());
    lane_two.merged_state = Some(stale_sibling_state);
    manager
        .save(&lane_two)
        .expect("seed stale integration metadata");
    let verification =
        heddle_output_env(&["--output", "json", "verify"], Some(fixture.path()), &[])
            .expect("run verification");
    assert!(
        !verification.status.success(),
        "fixture must start with blocked verification: {verification:?}"
    );
    let before_git = bytes_under(&fixture.path().join(".git"));
    let before_refs = bytes_under(&fixture.path().join(".heddle/refs"));
    let before_oplog = bytes_under(&fixture.path().join(".heddle/oplog"));
    let before_head = git_text(fixture.path(), &["rev-parse", "HEAD"]);
    let land = heddle_output_env(
        &["--output", "json", "land", "--thread", "lane/one"],
        Some(fixture.path()),
        &[],
    )
    .expect("attempt blocked land");
    assert!(
        !land.status.success(),
        "verification should block land: {land:?}"
    );
    assert_eq!(
        git_text(fixture.path(), &["rev-parse", "HEAD"]),
        before_head
    );
    assert_eq!(
        bytes_under(&fixture.path().join(".git")),
        before_git,
        "Git changed: {land:?}"
    );
    assert_eq!(
        bytes_under(&fixture.path().join(".heddle/refs")),
        before_refs,
        "Heddle refs changed: {land:?}"
    );
    assert_eq!(
        bytes_under(&fixture.path().join(".heddle/oplog")),
        before_oplog,
        "oplog changed: {land:?}"
    );
}

#[test]
fn sibling_refresh_never_reports_multiple_admitted_threads_without_recovery() {
    let (fixture, _, _) = r7_fanout();
    land_two(&fixture);
    let refresh = heddle_output_env(
        &["--output", "json", "thread", "refresh", "lane/one"],
        Some(fixture.path()),
        &[],
    )
    .expect("refresh sibling");
    if !refresh.status.success() {
        let error: Value =
            serde_json::from_slice(&refresh.stderr).expect("structured refresh error");
        assert!(
            error["primary_command"]
                .as_str()
                .is_some_and(|command| !command.is_empty()),
            "unrecoverable sibling refresh: {error}"
        );
        assert!(
            !error
                .to_string()
                .contains("local integration source revision is admitted on multiple Threads"),
            "spurious ambiguous source: {error}"
        );
    }
}
