// SPDX-License-Identifier: Apache-2.0
use super::*;

fn git(path: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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
        .expect("read ingest SHA map")
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

fn import_linear_git_history(path: &std::path::Path, commits: usize) {
    let mut child = Command::new("git")
        .args(["fast-import", "--quiet"])
        .current_dir(path)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(Stdio::piped())
        .spawn()
        .expect("start git fast-import");
    let mut stdin = child.stdin.take().expect("open git fast-import stdin");
    for generation in 1..=commits {
        let message = format!("commit {generation}");
        writeln!(stdin, "commit refs/heads/main").expect("write commit command");
        writeln!(stdin, "mark :{generation}").expect("write commit mark");
        writeln!(
            stdin,
            "author Deep Test <deep@test.invalid> {} +0000",
            1_700_000_000 + generation
        )
        .expect("write commit author");
        writeln!(
            stdin,
            "committer Deep Test <deep@test.invalid> {} +0000",
            1_700_000_000 + generation
        )
        .expect("write commit committer");
        writeln!(stdin, "data {}\n{message}", message.len()).expect("write commit message");
        if generation > 1 {
            writeln!(stdin, "from :{}", generation - 1).expect("write commit parent");
        }
        writeln!(stdin).expect("finish commit command");
    }
    writeln!(stdin, "done").expect("finish git fast-import stream");
    drop(stdin);
    let status = child.wait().expect("wait for git fast-import");
    assert!(status.success(), "git fast-import should succeed: {status}");
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
        .expect("read unchanged Git subtree mapping")
        .expect("unchanged Git subtree must retain its identity mapping");
    let stable_blob_hash = map
        .get_blob(&stable_git_blob)
        .expect("read unchanged Git blob mapping")
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
    heddle(&["import", "local"], Some(&work)).unwrap();

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
    heddle(&["bridge", "git", "import", "--ref", "main"], Some(&work)).unwrap();
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
    assert_eq!(
        git(&work, &["rev-parse", "refs/heads/main"]),
        new_git_tip,
        "authoritative local branch must advance to the upstream tip"
    );
    assert_eq!(
        git(&work, &["rev-parse", "refs/remotes/origin/main"]),
        new_git_tip,
        "authoritative remote-tracking ref must advance with the pull"
    );
    assert_eq!(
        git(&work, &["status", "--porcelain"]),
        "",
        "authoritative pull must leave the Git worktree clean"
    );

    let rerun = heddle_output(&["pull", "--output", "json"], Some(&work))
        .expect("repeat authoritative pull should run");
    assert!(
        rerun.status.success(),
        "repeat authoritative pull must succeed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rerun.stdout),
        String::from_utf8_lossy(&rerun.stderr)
    );
    assert_eq!(git(&work, &["rev-parse", "HEAD"]), new_git_tip);
    assert_eq!(git(&work, &["rev-parse", "refs/heads/main"]), new_git_tip);
    assert_eq!(
        git(&work, &["rev-parse", "refs/remotes/origin/main"]),
        new_git_tip
    );
    assert_eq!(git(&work, &["status", "--porcelain"]), "");
    let after_rerun = status_json(&work);
    assert_eq!(after_rerun["changes"], after["changes"]);
    assert_eq!(
        after_rerun["recommended_action"],
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
    let json = heddle(&["import", "local", "--output", "json"], Some(&work)).unwrap();
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

    let json = heddle(&["import", "local", "--output", "json"], Some(&work)).unwrap();
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

/// heddle#1791: cloning before adoption adds a remote-tracking ref, but the
/// attached `main` Thread must still receive the native identity hosted push
/// requires. The 0.23.0 path imported refs without creating that identity.
#[test]
fn cloned_git_repo_adoption_creates_main_native_identity_for_hosted_push() {
    let temp = TempDir::new().unwrap();
    let seed = temp.path().join("seed");
    let work = temp.path().join("work");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]);
    configure_git_identity(&seed);
    commit_file(&seed, "story.txt", "one\n", "seed");
    git(
        temp.path(),
        &["clone", seed.to_str().unwrap(), work.to_str().unwrap()],
    );

    heddle(&["import", "local"], Some(&work)).expect("adopt cloned Git repository");
    let adopted = repo::Repository::open(&work).expect("open adopted repository");
    adopted
        .native_thread("main")
        .expect("main native identity required by hosted push");
}

#[test]
fn adopt_roots_native_threads_at_the_hosted_seed_and_admits_git_history_as_captures() {
    use objects::object::thread_replication::{Admission, ThreadFacet, ThreadOperationBody};

    let temp = TempDir::new().unwrap();
    let work = temp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-b", "main"]);
    configure_git_identity(&work);
    let root_git = commit_file(&work, "story.txt", "one\n", "root by git author");
    git(&work, &["branch", "feature/root"]);
    let head_git = commit_file(&work, "story.txt", "one\ntwo\n", "head by git author");

    // A plain mechanical ingest is the control: Git's root has no content
    // parents, so seeding adoption must necessarily derive different IDs.
    let raw = temp.path().join("raw-import");
    let (_, raw_map) = ingest::import_git_into_scoped_with_options(
        &work,
        &raw,
        ingest::ImportOptions::default(),
        ingest::ImportScope::all(),
    )
    .expect("control import");
    let raw_root = raw_map
        .get_commit(&root_git)
        .expect("read raw root mapping")
        .expect("raw root mapping");
    let raw_head = raw_map
        .get_commit(&head_git)
        .expect("read raw head mapping")
        .expect("raw head mapping");
    let raw_repo = repo::Repository::open(&raw).expect("open control import");
    assert!(
        raw_repo
            .store()
            .get_state(&raw_root)
            .expect("read raw root")
            .expect("raw root state")
            .parents
            .is_empty(),
        "the control import must retain Git's parentless root"
    );

    heddle(&["import", "local"], Some(&work)).expect("adopt Git repository");
    let adopted = repo::Repository::open(&work).expect("open adopted repository");
    let adopted_map =
        ingest::ShaMap::open(work.join(".heddle/ingest/sha_map.sqlite")).expect("adopt map");
    let rooted_root = adopted_map
        .get_commit(&root_git)
        .expect("read adopted root mapping")
        .expect("adopted root mapping");
    let rooted_head = adopted_map
        .get_commit(&head_git)
        .expect("read adopted head mapping")
        .expect("adopted head mapping");
    let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()
        .expect("canonical hosted seed");
    let root_state = adopted
        .store()
        .get_state(&rooted_root)
        .expect("read rooted root")
        .expect("rooted root state");
    let head_state = adopted
        .store()
        .get_state(&rooted_head)
        .expect("read rooted head")
        .expect("rooted head state");
    let raw_head_state = raw_repo
        .store()
        .get_state(&raw_head)
        .expect("read raw head")
        .expect("raw head state");

    assert_eq!(root_state.parents, vec![seed.id()]);
    assert_ne!(rooted_root, raw_root, "re-parenting must re-hash the root");
    assert_ne!(
        rooted_head, raw_head,
        "the new root ID must cascade to HEAD"
    );
    assert_eq!(
        head_state.tree, raw_head_state.tree,
        "HEAD tree must survive"
    );
    assert_eq!(
        head_state.attribution, raw_head_state.attribution,
        "Git attribution must survive"
    );
    assert_eq!(head_state.attribution.principal.name_lossy(), "Heddle Test");
    assert_eq!(
        head_state.attribution.principal.email_lossy(),
        "heddle@example.com"
    );
    let tree = adopted
        .store()
        .get_tree(&head_state.tree)
        .expect("read adopted HEAD tree")
        .expect("adopted HEAD tree");
    let story = tree
        .entries()
        .iter()
        .find(|entry| entry.name() == "story.txt")
        .and_then(|entry| entry.blob_hash())
        .expect("story blob");
    assert_eq!(
        adopted
            .store()
            .get_blob(&story)
            .expect("read story blob")
            .expect("story blob present")
            .content(),
        b"one\ntwo\n"
    );

    for (name, tip) in [("main", rooted_head), ("feature/root", rooted_root)] {
        let replica = adopted
            .native_thread(name)
            .unwrap_or_else(|error| panic!("{name} native identity: {error}"));
        assert_eq!(
            replica.genesis().expect("native genesis").base,
            seed.id(),
            "{name} must use the canonical hosted root"
        );
        let operation_ids = replica
            .source_operation_page(tip, None, 1)
            .expect("source operation lookup");
        assert_eq!(operation_ids.len(), 1, "{name} tip must be admitted");
        let (signed, admission) = replica
            .operation(&operation_ids[0])
            .expect("load source operation")
            .expect("source operation present");
        assert_eq!(admission, Admission::Accepted);
        let operation = signed.verify().expect("valid source signature");
        assert!(
            matches!(operation.body, ThreadOperationBody::Capture(_)),
            "adopted Git history must use ordinary Capture operations"
        );
        assert_eq!(
            operation
                .source_state()
                .expect("decode source state")
                .expect("capture source state")
                .id(),
            tip
        );
        assert!(
            !replica
                .accepted_page(ThreadFacet::Source, None, 16)
                .expect("accepted source operations")
                .is_empty()
        );
    }
}

#[test]
fn adopt_deep_linear_history_registers_a_publishable_native_thread() {
    const COMMIT_COUNT: usize = 5_000;

    let temp = TempDir::new().expect("test directory");
    let work = temp.path().join("work");
    std::fs::create_dir(&work).expect("Git worktree");
    git(&work, &["init", "-b", "main"]);
    import_linear_git_history(&work, COMMIT_COUNT);
    let git_root = git(&work, &["rev-list", "--max-parents=0", "HEAD"]);
    let git_tip = git(&work, &["rev-parse", "HEAD"]);

    let output = heddle_output(&["import", "local", "--output", "json"], Some(&work))
        .expect("run deep-history adoption");
    assert!(
        output.status.success(),
        "deep-history adoption failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let adopted_output: Value =
        serde_json::from_slice(&output.stdout).expect("adopt output should be JSON");
    assert_eq!(adopted_output["commits_imported"], COMMIT_COUNT);
    assert_eq!(adopted_output["states_created"], COMMIT_COUNT);

    let adopted = repo::Repository::open(&work).expect("open adopted repository");
    let map = ingest::ShaMap::open(work.join(".heddle/ingest/sha_map.sqlite"))
        .expect("open adopted SHA map");
    let root = map
        .get_commit(&git_root)
        .expect("read root mapping")
        .expect("root state mapping");
    let tip = map
        .get_commit(&git_tip)
        .expect("read tip mapping")
        .expect("tip state mapping");
    let seed = objects::object::thread_replication::hosted_import::synthetic_initial_base()
        .expect("canonical hosted seed");
    let root_state = adopted
        .store()
        .get_state(&root)
        .expect("read adopted root")
        .expect("adopted root state");
    assert_eq!(root_state.parents, vec![seed.id()]);

    let replica = adopted.native_thread("main").expect("main native identity");
    assert_eq!(replica.genesis().expect("native genesis").base, seed.id());
    adopted
        .native_thread_signer(&replica)
        .expect("native Thread should retain its publishing signer");
    assert_eq!(
        replica
            .source_operation_page(tip, None, 1)
            .expect("tip source operation")
            .len(),
        1,
        "the adopted tip must be admitted for hosted publication"
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

    // Capture on main: write-through must parent onto the real Git tip.
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

    let new_git_tip = git(&work, &["rev-parse", "HEAD"]);
    let parent_of_new = git(&work, &["rev-parse", "HEAD^"]);
    assert_eq!(
        parent_of_new, main_tip,
        "write-through capture must parent the pre-bind Git tip (merge-base with main history); \
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

    heddle(&["import", "local"], Some(&work)).expect("full adoption after lazy bind");
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
    let human = heddle(&["import", "local"], Some(&work)).unwrap();
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
    let json = heddle(&["import", "local", "--output", "json"], Some(&work2)).unwrap();
    assert!(
        !json.contains('\r'),
        "adopt JSON leaked a carriage return: {json:?}"
    );
    assert!(
        !json.contains('\u{1b}'),
        "adopt JSON leaked an ANSI escape: {json:?}"
    );
}
