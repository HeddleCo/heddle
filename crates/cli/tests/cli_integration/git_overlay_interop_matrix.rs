// SPDX-License-Identifier: Apache-2.0
use super::*;

fn git(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn configure_git(path: &std::path::Path) {
    git(path, &["config", "user.name", "Interop User"]);
    git(path, &["config", "user.email", "interop@example.com"]);
}

fn init_git(path: &std::path::Path) {
    git(path, &["init"]);
    configure_git(path);
    git(path, &["switch", "-c", "main"]);
}

fn commit_file(path: &std::path::Path, file: &str, body: &str, message: &str) -> String {
    std::fs::write(path.join(file), body).unwrap();
    git(path, &["add", file]);
    git(path, &["commit", "-m", message]);
    git(path, &["rev-parse", "HEAD"])
}

#[test]
fn git_overlay_interop_bridge_shorthand_imports_current_branch() {
    let temp = TempDir::new().unwrap();
    init_git(temp.path());
    commit_file(temp.path(), "story.txt", "one\n", "seed");

    let status = heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    let parsed: Value = serde_json::from_str(&status).unwrap();
    assert_eq!(parsed["repository_capability"], "plain-git");
    assert!(parsed["current_state"].is_null());

    let import = heddle(
        &[
            "--output",
            "json",
            "bridge",
            "git",
            "import",
            "--ref",
            "main",
        ],
        Some(temp.path()),
    )
    .unwrap();
    let parsed_import: Value = serde_json::from_str(&import).unwrap_or(Value::Null);
    assert!(
        parsed_import["branches_synced"].as_u64() == Some(1)
            || import.contains("Synced 1 branches to threads"),
        "bridge shorthand should import branch: {import}"
    );

    let status = status_json(temp.path());
    assert!(
        status["current_state"].is_string(),
        "status after import: {status}"
    );
}

#[test]
fn git_overlay_interop_native_git_commit_then_heddle_import_adopts_tip() {
    let temp = TempDir::new().unwrap();
    init_git(temp.path());
    let first_tip = commit_file(temp.path(), "story.txt", "one\n", "seed");
    heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    heddle(&["bridge", "import", "--ref", "main"], Some(temp.path())).unwrap();
    let first_state = status_json(temp.path())["current_state"]
        .as_str()
        .unwrap()
        .to_string();

    let second_tip = commit_file(temp.path(), "story.txt", "one\ntwo\n", "native git commit");
    assert_ne!(first_tip, second_tip);
    let before_import = status_json(temp.path());
    assert_eq!(
        before_import["recommended_action"], "heddle adopt --ref main",
        "native git commit should require importing the active Git tip before more Heddle work: {before_import}"
    );

    heddle(&["bridge", "import", "--ref", "main"], Some(temp.path())).unwrap();
    let after_import = status_json(temp.path());
    assert_ne!(after_import["current_state"], first_state);
    assert_eq!(
        after_import["changes"]["modified"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_ne!(after_import["recommended_action"], "heddle capture");
}

#[test]
fn git_overlay_interop_fetch_sync_then_heddle_status_stays_clean() {
    let temp = TempDir::new().unwrap();
    let origin = temp.path().join("origin.git");
    let seed = temp.path().join("seed");
    let work = temp.path().join("work");
    let upstream = temp.path().join("upstream");

    git(temp.path(), &["init", "--bare", origin.to_str().unwrap()]);
    git(
        temp.path(),
        &["clone", origin.to_str().unwrap(), seed.to_str().unwrap()],
    );
    configure_git(&seed);
    git(&seed, &["switch", "-c", "main"]);
    commit_file(&seed, "story.txt", "one\n", "seed");
    git(&seed, &["push", "-u", "origin", "main"]);

    git(
        temp.path(),
        &["clone", origin.to_str().unwrap(), work.to_str().unwrap()],
    );
    configure_git(&work);
    git(&work, &["switch", "main"]);
    heddle(&["status", "--output", "json"], Some(&work)).unwrap();
    heddle(&["bridge", "import", "--ref", "main"], Some(&work)).unwrap();

    git(
        temp.path(),
        &[
            "clone",
            origin.to_str().unwrap(),
            upstream.to_str().unwrap(),
        ],
    );
    configure_git(&upstream);
    // The bare origin's HEAD still points to the pre-seed `master` ref
    // (which was never pushed), so git warns "remote HEAD refers to
    // nonexistent ref" and leaves the clone on an unborn `master`.
    // Explicitly switch to the only branch that actually exists so that
    // the subsequent commit lands on `main`, not `master`, and the push
    // of `origin main` succeeds on CI runners where init.defaultBranch
    // is not configured to `main`.
    git(&upstream, &["switch", "main"]);
    commit_file(&upstream, "story.txt", "one\ntwo\n", "advance upstream");
    git(&upstream, &["push", "origin", "main"]);
    git(&work, &["fetch", "origin"]);

    let sync = heddle(&["sync", "--output", "json"], Some(&work)).unwrap();
    let sync: Value = serde_json::from_str(&sync).unwrap();
    assert_eq!(sync["status"], "synced");

    let status = status_json(&work);
    assert_eq!(status["changes"]["modified"].as_array().unwrap().len(), 0);
    assert_eq!(status["changes"]["added"].as_array().unwrap().len(), 0);
    assert_ne!(status["recommended_action"], "heddle capture");
}

#[test]
fn git_overlay_interop_git_conflict_routes_to_no_git_handoff() {
    let temp = TempDir::new().unwrap();
    init_git(temp.path());
    commit_file(temp.path(), "clash.txt", "base\n", "seed");
    heddle(&["status", "--output", "json"], Some(temp.path())).unwrap();
    heddle(&["bridge", "import", "--ref", "main"], Some(temp.path())).unwrap();

    git(temp.path(), &["switch", "-c", "side"]);
    commit_file(temp.path(), "clash.txt", "side\n", "side change");
    git(temp.path(), &["switch", "main"]);
    commit_file(temp.path(), "clash.txt", "main\n", "main change");

    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(
        !merge.status.success(),
        "git merge should conflict\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&merge.stdout),
        String::from_utf8_lossy(&merge.stderr)
    );

    let status = status_json(temp.path());
    assert_eq!(status["operation"]["scope"], "git");
    assert_eq!(status["recommended_action"], "heddle bridge git status");

    let cont = heddle(&["--output", "text", "continue"], Some(temp.path())).unwrap_err();
    assert!(
        cont.contains("no-git runtime") && cont.contains("clash.txt"),
        "{cont}"
    );
    assert!(cont.contains("Next step:"));
}
