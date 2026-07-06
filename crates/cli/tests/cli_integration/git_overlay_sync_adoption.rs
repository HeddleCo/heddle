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

fn ingest_mapped_change(path: &std::path::Path, git_sha: &str) -> Option<String> {
    let map_path = path.join(".heddle").join("ingest").join("sha_map.sqlite");
    let map = ingest::ShaMap::open(map_path).expect("open ingest SHA map");
    map.get_commit(git_sha)
        .map(|change_id| change_id.to_string_full())
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

    heddle(&["status", "--output", "json"], Some(&work)).unwrap();
    heddle(&["import", "git", "--ref", "main"], Some(&work)).unwrap();
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

    let sync = heddle(&["sync", "--output", "json"], Some(&work)).unwrap();
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

    let import_again = heddle_output(&["import", "git", "--ref", "main"], Some(&work))
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

#[test]
fn adopt_renders_in_repo_paths_relative_to_repo_root() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    commit_file(&work, "story.txt", "one\n", "seed");

    // The .heddle data path lives inside the repo and must render relative
    // to the repo root, not as an absolute path that leaks the user's home
    // directory (#551).
    let json = heddle(&["adopt", "--output", "json"], Some(&work)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["path"], ".heddle");
    let abs = work.to_str().unwrap();
    assert!(
        !json.contains(&format!("{abs}/.heddle")),
        "adopt JSON should not contain an absolute in-repo path: {json}"
    );
}

#[test]
fn adopt_all_uses_ingest_mapping_without_internal_mirror() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    let git_tip = commit_file(&work, "story.txt", "one\n", "seed");

    let json = heddle(&["adopt", "--output", "json"], Some(&work)).unwrap();
    let value: Value = serde_json::from_str(&json).unwrap();

    assert_eq!(value["commits_imported"], 1);
    assert_eq!(value["states_created"], 1);
    assert!(
        !work.join(".heddle").join("git").exists(),
        "unscoped adopt should use ingest and avoid creating the legacy mirror"
    );
    let mapped_change = ingest_mapped_change(&work, &git_tip);
    assert!(
        mapped_change.as_ref().is_some_and(|id| !id.is_empty()),
        "adopt should persist the Git tip in the ingest identity map"
    );
    assert!(
        !work
            .join(".heddle")
            .join("git-projection")
            .join("git-projection-mapping.json")
            .exists(),
        "adopt/import must not publish the Git projection mapping cache"
    );
}

#[test]
fn adopt_emits_no_terminal_control_codes_in_piped_output() {
    let temp = TempDir::new().unwrap();

    // Human output, piped (non-TTY): no spinner carriage returns or ANSI
    // escapes should reach a non-terminal stdout (#550 progress AC).
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    commit_file(&work, "story.txt", "one\n", "seed");
    let human = heddle(&["adopt"], Some(&work)).unwrap();
    assert!(
        !human.contains('\r'),
        "human adopt output leaked a carriage return: {human:?}"
    );
    assert!(
        !human.contains('\u{1b}'),
        "human adopt output leaked an ANSI escape: {human:?}"
    );

    // JSON output: likewise free of live-progress control codes.
    let work2 = temp.path().join("work2");
    std::fs::create_dir(&work2).unwrap();
    git(&work2, &["init", "-b", "main"]);
    configure_git_identity(&work2);
    commit_file(&work2, "story.txt", "one\n", "seed");
    let json = heddle(&["adopt", "--output", "json"], Some(&work2)).unwrap();
    assert!(
        !json.contains('\r'),
        "adopt JSON leaked a carriage return: {json:?}"
    );
    assert!(
        !json.contains('\u{1b}'),
        "adopt JSON leaked an ANSI escape: {json:?}"
    );
}
