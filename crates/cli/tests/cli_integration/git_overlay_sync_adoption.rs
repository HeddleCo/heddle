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
        .map(|state_id| state_id.to_string_full())
}

fn native_mapped_object_files(path: &std::path::Path, state_id: &str) -> Vec<std::path::PathBuf> {
    let map = ingest::ShaMap::open(path.join(".heddle").join("ingest").join("sha_map.sqlite"))
        .expect("open overlay identity map");
    let objects = path.join(".heddle").join("objects");
    let mut files = vec![objects.join("states").join(format!("{state_id}.state"))];
    for (kind, directory) in [
        (ingest::MapKind::Blob, "blobs"),
        (ingest::MapKind::Tree, "trees"),
    ] {
        for hash in map.content_hashes(kind) {
            let hex = hash.to_hex();
            files.push(objects.join(directory).join(&hex[..2]).join(&hex[2..]));
        }
    }
    files.retain(|path| path.exists());
    files
}

#[test]
fn capture_persists_unchanged_git_subtree_and_blob_as_native_closure() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    std::fs::create_dir(work.join("stable")).unwrap();
    std::fs::write(work.join("stable/kept.txt"), "unchanged\n").unwrap();
    git(&work, &["add", "stable/kept.txt"]);
    commit_file(&work, "changed.txt", "before\n", "seed");
    let stable_git_tree = git(&work, &["rev-parse", "HEAD:stable"]);
    let stable_git_blob = git(&work, &["rev-parse", "HEAD:stable/kept.txt"]);
    heddle(&["init"], Some(&work)).unwrap();
    heddle(
        &["start", "feature/tree-reuse", "--workspace", "solid"],
        Some(&work),
    )
    .unwrap();

    std::fs::write(work.join("changed.txt"), "after\n").unwrap();
    heddle(&["capture", "-m", "change one file"], Some(&work)).unwrap();

    let map = ingest::ShaMap::open(work.join(".heddle/ingest/sha_map.sqlite")).unwrap();
    let stable_hash = map
        .get_tree(&stable_git_tree)
        .expect("unchanged Git subtree must retain its identity mapping");
    let stable_blob_hash = map
        .get_blob(&stable_git_blob)
        .expect("unchanged Git blob must retain its identity mapping");
    let captured_repo = repo::Repository::open(&work).unwrap();
    let captured = captured_repo
        .current_state()
        .unwrap()
        .expect("native capture state");
    assert!(
        captured_repo
            .store()
            .has_tree_locally(&captured.tree)
            .unwrap()
            && captured_repo
                .store()
                .has_tree_locally(&stable_hash)
                .unwrap(),
        "native capture must own its root and unchanged subtree"
    );
    assert!(
        captured_repo
            .store()
            .has_blob_locally(&stable_blob_hash)
            .unwrap(),
        "native capture must own unchanged blobs referenced by that subtree"
    );
    drop(captured_repo);

    // Simulate the exact Git-GC loss mode without relying on GC heuristics:
    // remove the loose source objects and prove a fresh native store can still
    // traverse the captured subtree and read its leaf.
    for oid in [&stable_git_tree, &stable_git_blob] {
        let object = work.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        assert!(object.is_file(), "fixture Git object must be loose: {oid}");
        std::fs::remove_file(object).unwrap();
    }
    let reopened = repo::Repository::open(&work).unwrap();
    let subtree = reopened
        .store()
        .get_tree(&stable_hash)
        .unwrap()
        .expect("unchanged subtree must survive loss of Git source object");
    let kept = subtree
        .entries()
        .iter()
        .find(|entry| entry.name() == "kept.txt")
        .and_then(|entry| entry.blob_hash())
        .expect("kept.txt blob hash");
    assert_eq!(kept, stable_blob_hash);
    assert_eq!(
        reopened
            .store()
            .get_blob(&kept)
            .unwrap()
            .expect("unchanged blob must survive loss of Git source object")
            .content(),
        b"unchanged\n"
    );
}

#[test]
fn native_adoption_does_not_fall_back_to_git_when_native_objects_are_missing() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    commit_file(&work, "story.txt", "native truth\n", "seed");
    heddle(&["init"], Some(&work)).unwrap();
    heddle(&["adopt"], Some(&work)).unwrap();

    let native = repo::Repository::open(&work).unwrap();
    let state_id = native
        .current_state()
        .unwrap()
        .expect("adopted current state")
        .state_id
        .to_string_full();
    for path in native_mapped_object_files(&work, &state_id) {
        std::fs::remove_file(path).unwrap();
    }

    for entry in std::fs::read_dir(work.join(".heddle/packs")).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            std::fs::remove_file(path).unwrap();
        }
    }
    let output = heddle_output(&["show", "HEAD", "--output", "json"], Some(&work)).unwrap();
    assert!(
        !output.status.success(),
        "native authority must report missing native state/tree/blob storage as corruption instead of reading through the retained SHA map into Git"
    );
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
fn init_then_ready_with_dirty_worktree_captures_edits_instead_of_binding_tip() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    let clean_tip = commit_file(&work, "story.txt", "committed\n", "seed main");
    heddle(&["init"], Some(&work)).unwrap();

    std::fs::write(work.join("story.txt"), "dirty worktree\n").unwrap();
    let ready_output = heddle_output(
        &["ready", "-m", "capture dirty bootstrap", "--output", "json"],
        Some(&work),
    )
    .expect("dirty bootstrap ready");
    let ready: Value = serde_json::from_slice(&ready_output.stdout).expect("ready JSON");
    assert_eq!(ready["captured"], true, "{ready}");
    assert!(
        ready["captured_state"]
            .as_str()
            .is_some_and(|state| state.starts_with("hs-")),
        "dirty bootstrap must report the native captured state: {ready}"
    );
    assert_eq!(
        ingest_mapped_change(&work, &clean_tip),
        None,
        "dirty bootstrap must not take the clean Git-tip descriptor path"
    );

    let repo = repo::Repository::open(&work).unwrap();
    let captured = repo
        .current_state()
        .unwrap()
        .expect("dirty bootstrap current state");
    assert_eq!(captured.intent.as_deref(), Some("capture dirty bootstrap"));
    let tree = repo
        .store()
        .get_tree(&captured.tree)
        .unwrap()
        .expect("captured dirty tree");
    let story = tree
        .entries()
        .iter()
        .find(|entry| entry.name() == "story.txt")
        .and_then(|entry| entry.blob_hash())
        .expect("captured story blob");
    assert_eq!(
        repo.store()
            .get_blob(&story)
            .unwrap()
            .expect("captured dirty blob")
            .content(),
        b"dirty worktree\n"
    );
}

/// P0-A: `heddle init` on an existing Git repo + `start` binds the active Git
/// tip through the authoritative `.git` database rather than copying its
/// state/tree/blob closure into native storage. The first export/write-through
/// must still share a merge-base with the base tip.
#[test]
fn init_then_start_binds_git_tip_not_orphan_bootstrap() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    let _root_tip = commit_file(&work, "story.txt", "one\n", "seed main");
    let main_tip = commit_file(&work, "story.txt", "one\ntwo\n", "advance main");

    heddle(&["init"], Some(&work)).unwrap();

    // start triggers ensure_current_state → lazy tip bind (not orphan bootstrap).
    heddle(
        &["start", "feature/agent-x", "--workspace", "solid"],
        Some(&work),
    )
    .unwrap();

    let mapped = ingest_mapped_change(&work, &main_tip)
        .expect("active Git tip must be mapped into the ingest SHA map");
    assert!(
        !mapped.is_empty(),
        "lazy tip bind should map the active Git tip"
    );
    let copied = native_mapped_object_files(&work, &mapped);
    assert!(
        copied.is_empty(),
        "lazy Git-tip binding must not copy mapped state, tree, or blob objects into the native store: {copied:?}"
    );
    assert_eq!(
        std::fs::read_dir(work.join(".heddle").join("packs"))
            .expect("native pack directory")
            .count(),
        0,
        "lazy Git-tip binding must not install a native object pack"
    );
    assert!(
        work.join(".heddle")
            .join("ingest")
            .join("overlay-states")
            .join(format!("{mapped}.state"))
            .is_file(),
        "the durable overlay identity descriptor should exist outside native object storage"
    );

    let log_json = heddle(&["log", "--output", "json"], Some(&work)).unwrap();
    let log: Value = serde_json::from_str(&log_json).expect("log json");
    let intents = log["states"]
        .as_array()
        .expect("log states array")
        .iter()
        .filter_map(|s| s.get("intent").and_then(|i| i.as_str()))
        .collect::<Vec<_>>();
    assert!(
        intents
            .iter()
            .all(|intent| !intent.contains("Bootstrap git-overlay")),
        "must not invent a synthetic Bootstrap git-overlay root when a Git tip exists; intents={intents:?}"
    );

    // Capture + commit on main: write-through must parent onto the real Git tip.
    std::fs::write(work.join("story.txt"), "one\ntwo\nmain-work\n").unwrap();
    heddle(&["capture", "-m", "main agent work"], Some(&work)).unwrap();
    let show: Value =
        serde_json::from_str(&heddle(&["show", "--output", "json"], Some(&work)).unwrap())
            .expect("show json");
    let parents = show["parents"].as_array().expect("parents array");
    assert!(
        !parents.is_empty(),
        "first capture after bind must parent the mapped Git tip, not be a parentless root: {show}"
    );
    assert!(
        parents.iter().any(|p| {
            p.as_str().is_some_and(|id| {
                mapped.starts_with(id) || id.starts_with(&mapped[..12.min(mapped.len())])
            })
        }),
        "parent should be the mapped tip {mapped}; parents={parents:?}"
    );

    heddle(&["commit", "-m", "main checkpoint"], Some(&work)).unwrap();
    let new_git_tip = git(&work, &["rev-parse", "HEAD"]);
    let parent_of_new = git(&work, &["rev-parse", "HEAD^"]);
    assert_eq!(
        parent_of_new, main_tip,
        "write-through commit must parent the pre-bind Git tip (merge-base with main history); \
         new={new_git_tip} parent={parent_of_new} expected={main_tip}"
    );
    let merge_base = git(&work, &["merge-base", &main_tip, &new_git_tip]);
    assert_eq!(
        merge_base, main_tip,
        "exported tip must share merge-base with the original main tip"
    );
}

#[test]
fn lazy_tip_log_cache_then_adopt_materializes_complete_native_graph() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    commit_file(&work, "story.txt", "one\n", "seed main");
    let main_tip = commit_file(&work, "story.txt", "one\ntwo\n", "advance main");

    heddle(&["init"], Some(&work)).unwrap();
    heddle(
        &["start", "feature/lazy-adopt", "--workspace", "solid"],
        Some(&work),
    )
    .unwrap();

    let lazy_state = ingest_mapped_change(&work, &main_tip).expect("lazy tip mapping");
    let short = &lazy_state[..12];
    let shown: Value = serde_json::from_str(
        &heddle(&["show", short, "--output", "json"], Some(&work))
            .expect("descriptor-backed short state id must resolve"),
    )
    .expect("show json");
    assert!(
        shown["state_id"]
            .as_str()
            .is_some_and(|id| id.starts_with(short)),
        "short-id show should return the lazy descriptor state: {shown}"
    );

    // Warm and persist the graph while only the non-root tip descriptor is
    // available. Its parent edge must remain unresolved rather than becoming
    // a cached zero-tree root.
    let lazy_log: Value = serde_json::from_str(
        &heddle(&["log", "--output", "json"], Some(&work)).expect("lazy descriptor log"),
    )
    .expect("lazy log json");
    assert_eq!(lazy_log["states"].as_array().map(Vec::len), Some(1));

    heddle(&["adopt"], Some(&work)).expect("full adoption after lazy bind");
    let native_state = ingest_mapped_change(&work, &main_tip).expect("native tip mapping");
    std::fs::rename(work.join(".git"), work.join(".git-disabled")).unwrap();

    let repo = repo::Repository::open(&work).expect("open adopted native repository");
    let state_id = objects::object::StateId::parse(&native_state).unwrap();
    let state = repo
        .store()
        .get_state(&state_id)
        .unwrap()
        .expect("adopted tip state must be native without Git read-through");
    let parent_id = *state
        .parents
        .first()
        .expect("non-root adopted tip must retain its real parent");
    let parent = repo
        .store()
        .get_state(&parent_id)
        .unwrap()
        .expect("adoption must materialize the previously unresolved parent");
    assert_ne!(
        parent.tree,
        objects::object::ContentHash::from_bytes([0; 32]),
        "the parent must not remain a cached zero-tree placeholder"
    );
    let mut graph = repo::CommitGraphIndex::new(&repo);
    assert!(
        graph.is_ancestor(&parent_id, &state_id).unwrap(),
        "reloaded graph must traverse the newly materialized parent"
    );
    assert_eq!(
        graph.find_merge_base(&parent_id, &state_id).unwrap(),
        Some(parent_id),
        "merge-base must see the real parent without manual cache rebuild"
    );
    let tree = repo
        .store()
        .get_tree(&state.tree)
        .unwrap()
        .expect("adopted tip tree must be native without Git read-through");
    let story = tree
        .entries()
        .iter()
        .find(|entry| entry.name() == "story.txt")
        .and_then(|entry| entry.blob_hash())
        .expect("story blob hash");
    let blob = repo
        .store()
        .get_blob(&story)
        .unwrap()
        .expect("adopted tip blob must be native without Git read-through");
    assert_eq!(blob.content(), b"one\ntwo\n");

    let path_log: Value = serde_json::from_str(
        &heddle(
            &["log", "--path", "story.txt", "--output", "json"],
            Some(&work),
        )
        .expect("path history after materialization"),
    )
    .expect("path log json");
    assert_eq!(
        path_log["states"].as_array().map(Vec::len),
        Some(2),
        "path history must cross the formerly unresolved parent: {path_log}"
    );
}

#[test]
fn initialized_overlay_two_sided_head_diff_binds_git_tip() {
    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    commit_file(&work, "story.txt", "one\n", "seed main");

    heddle(&["init"], Some(&work)).unwrap();
    let diff: Value = serde_json::from_str(
        &heddle(&["diff", "HEAD", "HEAD", "--output", "json"], Some(&work))
            .expect("two-sided HEAD diff must lazily bind the Git tip"),
    )
    .expect("diff json");
    assert_eq!(diff["stats"]["files_changed"], 0, "{diff}");

    let repo = repo::Repository::open(&work).unwrap();
    assert!(
        repo.current_state().unwrap().is_some(),
        "two-sided HEAD resolution must bind the authoritative Git tip"
    );
}

#[test]
fn tip_bind_distinguishes_unborn_head_from_corrupt_head() {
    let temp = TempDir::new().unwrap();
    let unborn = temp.path().join("unborn");
    std::fs::create_dir(&unborn).unwrap();
    git(&unborn, &["init", "-b", "main"]);
    configure_git_identity(&unborn);
    heddle(&["init"], Some(&unborn)).unwrap();
    heddle(
        &["start", "feature/unborn", "--workspace", "solid"],
        Some(&unborn),
    )
    .expect("a genuine unborn HEAD may bootstrap");

    let corrupt = temp.path().join("corrupt");
    std::fs::create_dir(&corrupt).unwrap();
    git(&corrupt, &["init", "-b", "main"]);
    configure_git_identity(&corrupt);
    commit_file(&corrupt, "story.txt", "seed\n", "seed");
    heddle(&["init"], Some(&corrupt)).unwrap();
    std::fs::write(corrupt.join(".git").join("HEAD"), "not a valid HEAD\n").unwrap();

    let output = heddle_output(
        &[
            "--output",
            "json",
            "start",
            "feature/corrupt",
            "--workspace",
            "solid",
        ],
        Some(&corrupt),
    )
    .expect("invoke start against corrupt HEAD");
    assert!(!output.status.success());
    let error: Value = serde_json::from_slice(&output.stderr).expect("typed JSON failure");
    assert_eq!(error["kind"], "git_overlay_tip_bind_failed", "{error}");
    assert!(
        error["unsafe_condition"]
            .as_str()
            .is_some_and(|detail| detail.contains("failed to resolve Git HEAD")),
        "unexpected error: {error}"
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
