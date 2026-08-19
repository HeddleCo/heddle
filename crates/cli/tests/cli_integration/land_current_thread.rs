// SPDX-License-Identifier: Apache-2.0
//! Bare `land` from an isolated native checkout must land the current
//! thread, not the target `main` ref (heddle#1436).

use std::{fs, path::PathBuf, str};

use serde_json::Value;
use tempfile::TempDir;

use super::{heddle, heddle_output};

fn sibling_checkout_path(repo: &std::path::Path, suffix: &str) -> PathBuf {
    let repo_name = repo
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    repo.with_file_name(format!("{repo_name}-{suffix}"))
}

fn json_value(cwd: &std::path::Path, args: &[&str]) -> Value {
    let stdout = heddle(args, Some(cwd)).unwrap_or_else(|err| panic!("{}: {err}", args.join(" ")));
    serde_json::from_str(&stdout)
        .unwrap_or_else(|err| panic!("JSON from `{}`: {err}\n{stdout}", args.join(" ")))
}

fn setup_isolated_native_thread(thread: &str, file: &str, contents: &str) -> (TempDir, PathBuf) {
    let temp = TempDir::new().unwrap();
    heddle(&["init"], Some(temp.path())).unwrap();
    fs::write(temp.path().join("seed.txt"), "seed\n").unwrap();
    heddle(&["capture", "-m", "seed"], Some(temp.path())).unwrap();

    let checkout = sibling_checkout_path(temp.path(), "search");
    let checkout_arg = checkout.to_str().expect("checkout path utf8");
    heddle(
        &["start", thread, "--path", checkout_arg],
        Some(temp.path()),
    )
    .unwrap();
    fs::write(checkout.join(file), contents).unwrap();
    heddle(&["capture", "-m", "feature work"], Some(&checkout)).unwrap();
    heddle(&["ready"], Some(&checkout)).unwrap();
    (temp, checkout)
}

#[test]
fn bare_land_from_isolated_native_checkout_lands_current_thread() {
    let (main, checkout) =
        setup_isolated_native_thread("feature/search", "search.txt", "find it\n");

    let dry = json_value(&checkout, &["--output", "json", "land", "--dry-run"]);
    assert_eq!(dry["command"], "land", "{dry}");
    assert!(
        dry["summary"].as_str().is_some_and(
            |summary| summary.contains("feature/search") && !summary.contains("'main'")
        ),
        "bare land --dry-run must preview the current thread, not main: {dry}"
    );
    assert_eq!(dry["integrations"][0]["thread"], "feature/search", "{dry}");
    assert_eq!(dry["integrations"][0]["target"], "main", "{dry}");
    assert_eq!(dry["side_effects"]["writes_git_refs"], false, "{dry}");
    assert_eq!(dry["side_effects"]["network_io"], false, "{dry}");
    assert!(
        dry["blockers"]
            .as_array()
            .is_none_or(|blockers| blockers.is_empty()),
        "current-thread dry-run should not invent a missing-main blocker: {dry}"
    );

    let landed = json_value(&checkout, &["--output", "json", "land"]);
    assert_eq!(landed["status"], "landed", "{landed}");
    assert_eq!(landed["thread"], "feature/search", "{landed}");
    assert_eq!(landed["integrated"], true, "{landed}");
    assert_eq!(
        fs::read_to_string(main.path().join("search.txt")).unwrap(),
        "find it\n"
    );

    let replay = json_value(&checkout, &["--output", "json", "land"]);
    assert_eq!(replay["status"], "already_landed", "{replay}");
    assert_eq!(replay["thread"], "feature/search", "{replay}");
    assert_eq!(replay["integrated"], false, "{replay}");
    assert_eq!(replay["chosen_path"], "already_integrated", "{replay}");
    assert!(
        replay["message"]
            .as_str()
            .is_some_and(|message| message.contains("already landed")),
        "second land must name already-landed, not a second merge: {replay}"
    );
    assert!(
        !replay["message"]
            .as_str()
            .is_some_and(|message| message.contains("automatic integration merge")),
        "second land must not claim a new automatic merge: {replay}"
    );
    let next = replay["next_action"].as_str().unwrap_or("");
    assert!(
        !next.contains("repair git") && !next.contains("fsck"),
        "already-landed next action must stay native: {replay}"
    );

    let replay_dry = json_value(&checkout, &["--output", "json", "land", "--dry-run"]);
    assert_eq!(
        replay_dry["integrations"][0]["would_transition_to"], "already_landed",
        "{replay_dry}"
    );
}

#[test]
fn land_unmanaged_main_on_native_recommends_thread_list() {
    let (_main, checkout) =
        setup_isolated_native_thread("feature/search", "search.txt", "find it\n");

    let output = heddle_output(
        &["--output", "json", "land", "--thread", "main"],
        Some(&checkout),
    )
    .expect("land --thread main should spawn");
    assert_ne!(
        output.status.code(),
        Some(0),
        "landing unmanaged main must fail closed"
    );
    let stderr = str::from_utf8(&output.stderr).unwrap_or("");
    let envelope: Value = serde_json::from_str(
        stderr
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or(stderr.trim()),
    )
    .unwrap_or_else(|err| panic!("JSON envelope: {err}\n{stderr}"));
    assert_eq!(envelope["kind"], "imported_git_ref_not_managed_thread");
    assert_eq!(envelope["primary_command"], "heddle thread list");
    assert!(
        !stderr.contains("repair git")
            && !envelope["primary_command"]
                .as_str()
                .is_some_and(|command| command.contains("repair git")),
        "native authority must not recommend repair git: {envelope}"
    );
}
