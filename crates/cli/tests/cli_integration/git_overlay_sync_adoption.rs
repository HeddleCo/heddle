// SPDX-License-Identifier: Apache-2.0
use super::*;

fn git(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap_or_else(|err| panic!("git {:?} should run: {}", args, err));
    assert!(
        output.status.success(),
        "git {:?} should succeed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn configure_git_identity(path: &std::path::Path) {
    git(path, &["config", "user.name", "Heddle Test"]);
    git(path, &["config", "user.email", "heddle@example.com"]);
}

fn commit_file(path: &std::path::Path, file: &str, body: &str, message: &str) -> String {
    std::fs::write(path.join(file), body).unwrap();
    git(path, &["add", file]);
    git(path, &["commit", "-m", message]);
    git(path, &["rev-parse", "HEAD"])
}

#[test]
fn git_overlay_sync_adopts_fast_forward_upstream_tip() {
    let temp = TempDir::new().unwrap();
    let seed = temp.path().join("seed");
    let origin = temp.path().join("origin.git");
    let work = temp.path().join("work");
    let upstream = temp.path().join("upstream");

    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    configure_git_identity(&seed);
    commit_file(&seed, "story.txt", "one\n", "seed main");
    git(
        temp.path(),
        &[
            "clone",
            "--bare",
            seed.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    git(
        temp.path(),
        &["clone", origin.to_str().unwrap(), work.to_str().unwrap()],
    );
    configure_git_identity(&work);

    heddle(&["status", "--json"], Some(&work)).unwrap();
    heddle(&["bridge", "import", "--ref", "main"], Some(&work)).unwrap();
    let before = status_json(&work);
    let before_state = before["current_state"]
        .as_str()
        .expect("imported current_state")
        .to_string();

    git(
        temp.path(),
        &[
            "clone",
            origin.to_str().unwrap(),
            upstream.to_str().unwrap(),
        ],
    );
    configure_git_identity(&upstream);
    let new_git_tip = commit_file(&upstream, "story.txt", "one\ntwo\n", "advance main");
    git(&upstream, &["push", "origin", "main"]);
    git(&work, &["fetch", "origin"]);

    let sync = heddle(&["sync", "--json"], Some(&work)).unwrap();
    let sync_json: Value = serde_json::from_str(&sync).expect("sync output should be JSON");
    assert_eq!(
        sync_json["status"], "synced",
        "sync should pull/adopt: {sync_json}"
    );
    assert!(
        sync_json["recommended_action"].is_null(),
        "fast-forward sync should not recommend capture: {sync_json}"
    );

    let after = status_json(&work);
    assert_eq!(after["thread"], "main");
    assert_ne!(after["current_state"], before_state);
    assert_eq!(after["changes"]["modified"].as_array().unwrap().len(), 0);
    assert_eq!(after["changes"]["added"].as_array().unwrap().len(), 0);
    assert_eq!(after["changes"]["deleted"].as_array().unwrap().len(), 0);
    assert_ne!(after["recommended_action"], "heddle capture");
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), new_git_tip);

    let import_again = heddle_output(&["bridge", "import", "--ref", "main"], Some(&work))
        .expect("import command should run");
    assert!(
        import_again.status.success(),
        "re-importing the adopted fast-forward tip should be a clean no-op\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&import_again.stdout),
        String::from_utf8_lossy(&import_again.stderr)
    );
    let after_reimport = status_json(&work);
    assert_eq!(after_reimport["current_state"], after["current_state"]);
    assert_eq!(
        after_reimport["changes"]["modified"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        after_reimport["recommended_action"],
        after["recommended_action"]
    );
}
