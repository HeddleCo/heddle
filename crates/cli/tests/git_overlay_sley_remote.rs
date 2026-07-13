// SPDX-License-Identifier: Apache-2.0

use std::{
    io::Write as _,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::Value;
use sley::{
    CommitObject, DeleteRef, EntryKind, GitObjectType, ObjectId, RefPrecondition, ReferenceTarget,
    Repository as SleyRepository,
    plumbing::{sley_object::EncodedObject, sley_refs::ReflogEntry},
};
use tempfile::TempDir;

fn write_commit(
    repo: &SleyRepository,
    parent: Option<ObjectId>,
    content: &[u8],
    message: &[u8],
) -> ObjectId {
    let blob = repo.write_blob(content).expect("write blob");
    let empty = repo
        .write_tree(sley::TreeEditor::new())
        .expect("write empty tree");
    let mut tree = repo.edit_tree(&empty).expect("edit tree");
    tree.upsert("tracked.txt", EntryKind::Blob, blob);
    let tree = repo.write_tree(tree).expect("write populated tree");
    let identity = b"Heddle Test <heddle@example.com> 0 +0000".to_vec();
    let commit = CommitObject {
        tree,
        parents: parent.into_iter().collect(),
        author: identity.clone(),
        committer: identity.clone(),
        encoding: None,
        message: message.to_vec(),
    };
    repo.write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("write commit")
}

fn publish_branch(repo: &SleyRepository, branch: &str, old: Option<ObjectId>, new: ObjectId) {
    publish_ref(repo, &format!("refs/heads/{branch}"), old, new);
}

fn publish_ref(repo: &SleyRepository, name: &str, old: Option<ObjectId>, new: ObjectId) {
    let identity = b"Heddle Test <heddle@example.com> 0 +0000".to_vec();
    let references = repo.references();
    let mut refs = references.transaction();
    refs.update_to(
        name,
        ReferenceTarget::Direct(new),
        RefPrecondition::Any,
        Some(ReflogEntry {
            old_oid: old.unwrap_or_else(|| ObjectId::null(repo.object_format())),
            new_oid: new,
            committer: identity,
            message: format!("update {name}").into_bytes(),
        }),
    );
    refs.commit().expect("publish branch");
}

fn delete_ref(repo: &SleyRepository, name: &str, expected_old: ObjectId) {
    repo.delete_ref(DeleteRef {
        name: sley::FullName::new(name).expect("valid ref name"),
        expected_old: Some(expected_old),
        expected: None,
        reflog: None,
        reflog_committer: None,
    })
    .expect("delete ref");
}

fn seed_source(path: &Path) -> (SleyRepository, ObjectId) {
    let repo = SleyRepository::init_bare(path).expect("initialize source");
    let first = write_commit(&repo, None, b"one\n", b"one\n");
    publish_branch(&repo, "main", None, first);
    std::fs::write(path.join("HEAD"), b"ref: refs/heads/main\n").expect("write source HEAD");
    (repo, first)
}

fn config(temp: &TempDir) -> std::path::PathBuf {
    let path = temp.path().join("heddle-config.toml");
    std::fs::write(
        &path,
        "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n",
    )
    .expect("write config");
    path
}

fn run(temp: &TempDir, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(args)
        .current_dir(cwd)
        .env("PATH", "")
        .env("HOME", temp.path())
        .env("HEDDLE_CONFIG", config(temp))
        .env("NO_COLOR", "1")
        .output()
        .expect("run Heddle without PATH lookup")
}

fn clone_source(temp: &TempDir, source: &Path, checkout: &Path) {
    let output = run(
        temp,
        temp.path(),
        &[
            "--output",
            "json",
            "clone",
            source.to_str().expect("source UTF-8"),
            checkout.to_str().expect("checkout UTF-8"),
        ],
    );
    assert!(
        output.status.success(),
        "clone failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn link_overlay_to_hosted(checkout: &Path) {
    let path = checkout.join(".heddle/config.toml");
    let mut config = repo::RepoConfig::load(&path).expect("load repository config");
    config.hosted.upstream_url = Some("https://hosted.example.test".to_string());
    config.hosted.namespace = Some("acme/widget".to_string());
    config.save(&path).expect("save hosted linkage");
}

#[test]
fn embedded_credential_store_runs_without_git_on_path() {
    let temp = TempDir::new().expect("tempdir");
    let credentials = temp.path().join("credentials");
    let helper_args = [
        "credential-store".to_string(),
        format!("--file={}", credentials.display()),
    ];

    let mut store = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(&helper_args)
        .arg("store")
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start embedded credential store");
    store
        .stdin
        .take()
        .expect("store stdin")
        .write_all(b"protocol=https\nhost=example.test\nusername=alice\npassword=secret\n\n")
        .expect("write credential");
    let stored = store.wait_with_output().expect("wait for store");
    assert!(
        stored.status.success(),
        "embedded store failed: {}",
        String::from_utf8_lossy(&stored.stderr)
    );

    let mut get = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args(&helper_args)
        .arg("get")
        .env("PATH", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start embedded credential lookup");
    get.stdin
        .take()
        .expect("get stdin")
        .write_all(b"protocol=https\nhost=example.test\n\n")
        .expect("write lookup");
    let found = get.wait_with_output().expect("wait for lookup");
    assert!(
        found.status.success(),
        "embedded lookup failed: {}",
        String::from_utf8_lossy(&found.stderr)
    );
    let found = String::from_utf8(found.stdout).expect("credential output UTF-8");
    assert!(found.contains("username=alice"));
    assert!(found.contains("password=secret"));
}

fn set_git_config(repo: &SleyRepository, key: &str, value: &str) {
    let edit = sley::ConfigEditPlan::new(repo.common_dir().join("config"))
        .with_operation(sley::ConfigEdit::set(key, value).expect("create Git config edit"));
    repo.apply_config_edit_plan(edit)
        .expect("write local Git config");
}

#[test]
fn pull_streams_and_fast_forwards_with_empty_process_path() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (source, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);

    let local = SleyRepository::discover(&checkout).expect("open checkout");
    set_git_config(&local, "branch.main.merge", "refs/heads/release");

    let second = write_commit(&source, Some(first), b"two\n", b"two\n");
    publish_branch(&source, "release", None, second);

    let output = run(&temp, &checkout, &["--output", "json", "pull"]);
    assert!(
        output.status.success(),
        "pull must not require the git executable\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("pull JSON");
    assert_eq!(result["transport"], "git");
    assert_eq!(result["new_git_head"], second.to_string());
    assert_eq!(
        std::fs::read_to_string(checkout.join("tracked.txt")).expect("materialized file"),
        "two\n"
    );
    let local = SleyRepository::discover(&checkout).expect("reopen checkout");
    assert_eq!(local.head().expect("read HEAD").oid, Some(second));
    assert!(!checkout.join(".heddle/git").exists());
}

#[test]
fn hosted_linked_overlay_pushes_explicit_git_remote_through_sley() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (source, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);
    link_overlay_to_hosted(&checkout);

    let local = SleyRepository::discover(&checkout).expect("open checkout");
    let second = write_commit(&local, Some(first), b"two\n", b"two\n");
    publish_branch(&local, "main", Some(first), second);
    std::fs::write(checkout.join("tracked.txt"), b"two\n").expect("materialize local commit");

    let pushed = run(&temp, &checkout, &["--output", "json", "push", "origin"]);
    assert!(
        pushed.status.success(),
        "explicit Git push must ignore repository-wide hosted linkage\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    );
    let pushed: Value = serde_json::from_slice(&pushed.stdout).expect("push JSON");
    assert_eq!(pushed["transport"], "git");
    assert_eq!(
        source
            .find_reference("refs/heads/main")
            .expect("read remote main")
            .expect("remote main")
            .direct_target()
            .expect("direct remote main"),
        second
    );
}

#[test]
fn hosted_linked_overlay_pulls_explicit_git_remote_through_sley() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (source, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);
    link_overlay_to_hosted(&checkout);

    let second = write_commit(&source, Some(first), b"two\n", b"two\n");
    publish_branch(&source, "main", Some(first), second);

    let pulled = run(&temp, &checkout, &["--output", "json", "pull", "origin"]);
    assert!(
        pulled.status.success(),
        "explicit Git pull must ignore repository-wide hosted linkage\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pulled.stdout),
        String::from_utf8_lossy(&pulled.stderr)
    );
    let pulled: Value = serde_json::from_slice(&pulled.stdout).expect("pull JSON");
    assert_eq!(pulled["transport"], "git");
    assert_eq!(pulled["new_git_head"], second.to_string());
    assert_eq!(
        std::fs::read_to_string(checkout.join("tracked.txt")).expect("materialized file"),
        "two\n"
    );
}

#[test]
fn overlay_pull_uses_url_even_when_pushurl_is_hosted() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (source, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);

    let local = SleyRepository::discover(&checkout).expect("open checkout");
    set_git_config(
        &local,
        "remote.origin.pushurl",
        "heddle://127.0.0.1:1/acme/widget",
    );
    let second = write_commit(&source, Some(first), b"two\n", b"two\n");
    publish_branch(&source, "main", Some(first), second);

    let pulled = run(&temp, &checkout, &["--output", "json", "pull", "origin"]);
    assert!(
        pulled.status.success(),
        "pull must use remote.origin.url, not its hosted pushurl\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pulled.stdout),
        String::from_utf8_lossy(&pulled.stderr)
    );
    let pulled: Value = serde_json::from_slice(&pulled.stdout).expect("pull JSON");
    assert_eq!(pulled["transport"], "git");
    assert_eq!(pulled["new_git_head"], second.to_string());
}

#[test]
fn overlay_push_uses_pushurl_even_when_url_is_hosted() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (source, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);

    let local = SleyRepository::discover(&checkout).expect("open checkout");
    set_git_config(
        &local,
        "remote.origin.url",
        "heddle://127.0.0.1:1/acme/widget",
    );
    set_git_config(
        &local,
        "remote.origin.pushurl",
        source_path.to_str().expect("source path UTF-8"),
    );
    let second = write_commit(&local, Some(first), b"two\n", b"two\n");
    publish_branch(&local, "main", Some(first), second);
    std::fs::write(checkout.join("tracked.txt"), b"two\n").expect("materialize local commit");

    let pushed = run(&temp, &checkout, &["--output", "json", "push", "origin"]);
    assert!(
        pushed.status.success(),
        "push must use remote.origin.pushurl, not its hosted fetch URL\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    );
    let pushed: Value = serde_json::from_slice(&pushed.stdout).expect("push JSON");
    assert_eq!(pushed["transport"], "git");
    assert_eq!(
        source
            .find_reference("refs/heads/main")
            .expect("read remote main")
            .expect("remote main")
            .direct_target()
            .expect("direct remote main"),
        second
    );
}

#[test]
fn overlay_pull_requires_a_configured_remote_before_streaming() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (_, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);

    let output = run(
        &temp,
        &checkout,
        &[
            "--output",
            "json",
            "pull",
            source_path.to_str().expect("source UTF-8"),
        ],
    );
    assert!(!output.status.success(), "direct URL pull must be refused");
    assert!(output.stdout.is_empty(), "JSON errors keep stdout clean");
    let refusal: Value = serde_json::from_slice(&output.stderr).expect("structured refusal JSON");
    assert_eq!(
        refusal["kind"],
        "git_overlay_pull_requires_configured_remote"
    );
    let local = SleyRepository::discover(&checkout).expect("open checkout");
    assert_eq!(local.head().expect("read HEAD").oid, Some(first));
}

#[test]
fn overlay_remote_commands_mutate_only_git_config_without_git_executable() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let backup = temp.path().join("backup.git");
    let checkout = temp.path().join("checkout");
    seed_source(&source_path);
    SleyRepository::init_bare(&backup).expect("initialize backup");
    clone_source(&temp, &source_path, &checkout);
    let git = SleyRepository::discover(&checkout).expect("open checkout");
    set_git_config(&git, "branch.main.merge", "refs/heads/release");

    for args in [
        vec![
            "remote",
            "add",
            "backup",
            backup.to_str().expect("backup UTF-8"),
        ],
        vec!["remote", "list"],
        vec!["remote", "show", "backup"],
        vec!["remote", "set-default", "backup"],
    ] {
        let output = run(&temp, &checkout, &args);
        assert!(
            output.status.success(),
            "{args:?} failed without git\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let git = SleyRepository::discover(&checkout).expect("open checkout");
    let snapshot = git.config_snapshot().expect("read Git config");
    assert_eq!(
        snapshot.get("remote", Some("backup"), "url"),
        backup.to_str()
    );
    assert_eq!(snapshot.get("remote", None, "pushDefault"), Some("backup"));
    assert_eq!(
        snapshot.get("branch", Some("main"), "remote"),
        Some("backup")
    );
    assert_eq!(
        snapshot.get("branch", Some("main"), "merge"),
        Some("refs/heads/release")
    );
    assert!(
        !checkout.join(".heddle/remotes.toml").exists(),
        "Git Overlay remote commands must not create native Heddle remote config"
    );

    let remove = run(&temp, &checkout, &["remote", "remove", "backup"]);
    assert!(
        remove.status.success(),
        "remove failed without git: {}",
        String::from_utf8_lossy(&remove.stderr)
    );
    let git = SleyRepository::discover(&checkout).expect("reopen checkout");
    assert!(
        !git.remote_names()
            .expect("list remotes")
            .contains(&"backup".to_string())
    );
}

#[test]
fn overlay_push_rejects_native_only_flags_before_transport() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);

    for (args, kind) in [
        (
            vec!["--output", "json", "push", "--state", "deadbeef"],
            "git_overlay_push_state_unsupported",
        ),
        (
            vec!["--output", "json", "push", "--insecure"],
            "git_overlay_push_insecure_unsupported",
        ),
    ] {
        let output = run(&temp, &checkout, &args);
        assert!(!output.status.success(), "{args:?} must be refused");
        assert!(output.stdout.is_empty(), "JSON errors keep stdout clean");
        let refusal: Value =
            serde_json::from_slice(&output.stderr).expect("structured refusal JSON");
        assert_eq!(refusal["kind"], kind);
        assert!(refusal["preserved"].as_str().is_some_and(|text| {
            text.contains("no hook ran") && text.contains("left unchanged")
        }));
    }
}

#[test]
fn overlay_push_all_threads_carries_git_refs_and_spares_foreign_destination_refs() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (source, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);

    let local = SleyRepository::discover(&checkout).expect("open checkout");
    let feature = write_commit(&local, Some(first), b"feature\n", b"feature\n");
    publish_branch(&local, "feature", None, feature);
    publish_branch(&local, "foreign-collision", None, first);
    publish_branch(&source, "foreign-collision", None, first);
    publish_ref(&local, "refs/tags/v1", None, feature);
    heddle_git_projection::git_notes::write_note(
        &local,
        feature,
        &heddle_git_projection::git_notes::HeddleNote {
            source_state: None,
            state_id: "hs-test-state".to_string(),
            change_id: "hc-test-change".to_string(),
            agent: None,
            confidence: Some(0.9),
            status: "published".to_string(),
            omitted_annotations_breakdown: None,
            signal_counts: None,
            attribution: None,
        },
    )
    .expect("write Heddle note");

    let pushed = run(
        &temp,
        &checkout,
        &["--output", "json", "push", "--all-threads"],
    );
    assert!(
        pushed.status.success(),
        "all-thread push failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pushed.stdout),
        String::from_utf8_lossy(&pushed.stderr)
    );
    let pushed: Value = serde_json::from_slice(&pushed.stdout).expect("push JSON");
    assert_eq!(pushed["ref_scope"], "all_threads_tags_and_heddle_notes");
    assert_eq!(pushed["git_notes_ref"], "refs/notes/heddle");
    assert_eq!(pushed["tags_included"], true);
    assert!(
        pushed["refs_written"].as_array().is_some_and(|refs| refs
            .iter()
            .any(|name| name == "refs/heads/feature")
            && refs.iter().any(|name| name == "refs/tags/v1")
            && refs.iter().any(|name| name == "refs/notes/heddle")),
        "push must report every materialized namespace: {pushed}"
    );
    assert!(
        source
            .find_reference("refs/heads/feature")
            .unwrap()
            .is_some()
    );
    assert!(source.find_reference("refs/tags/v1").unwrap().is_some());
    assert!(
        source
            .find_reference("refs/heads/foreign-collision")
            .unwrap()
            .is_some()
    );
    assert!(
        source
            .find_reference("refs/notes/heddle")
            .unwrap()
            .is_some()
    );

    let advanced_feature = write_commit(&local, Some(feature), b"advanced\n", b"advanced\n");
    publish_branch(&local, "feature", Some(feature), advanced_feature);
    publish_ref(&local, "refs/tags/v1", Some(feature), advanced_feature);
    let scoped = run(&temp, &checkout, &["--output", "json", "push"]);
    assert!(
        scoped.status.success(),
        "current-thread push failed: {}",
        String::from_utf8_lossy(&scoped.stderr)
    );
    assert_eq!(
        source
            .find_reference("refs/heads/feature")
            .unwrap()
            .expect("remote feature survives")
            .direct_target()
            .expect("direct feature target"),
        feature,
        "current-thread push must not advance an existing sibling branch"
    );
    assert_eq!(
        source
            .find_reference("refs/tags/v1")
            .unwrap()
            .expect("remote tag survives")
            .direct_target()
            .expect("direct tag target"),
        feature,
        "current-thread push must not update an existing tag"
    );

    delete_ref(&local, "refs/heads/feature", advanced_feature);
    delete_ref(&local, "refs/heads/foreign-collision", first);
    delete_ref(&local, "refs/tags/v1", advanced_feature);
    let retracted = run(
        &temp,
        &checkout,
        &["--output", "json", "push", "--all-threads"],
    );
    assert!(
        retracted.status.success(),
        "retraction push failed: {}",
        String::from_utf8_lossy(&retracted.stderr)
    );
    assert!(
        source
            .find_reference("refs/heads/feature")
            .unwrap()
            .is_none()
    );
    assert!(source.find_reference("refs/tags/v1").unwrap().is_none());
    assert!(
        source
            .find_reference("refs/heads/foreign-collision")
            .unwrap()
            .is_some(),
        "a same-name destination ref Heddle never wrote must remain unowned"
    );
    assert!(
        source
            .find_reference("refs/notes/heddle")
            .unwrap()
            .is_some(),
        "still-served provenance notes must survive sibling ref deletion"
    );
    assert!(!checkout.join(".heddle/git").exists());
}

#[test]
fn overlay_pull_fetches_heddle_notes_with_the_branch() {
    let temp = TempDir::new().expect("tempdir");
    let source_path = temp.path().join("source.git");
    let checkout = temp.path().join("checkout");
    let (source, first) = seed_source(&source_path);
    clone_source(&temp, &source_path, &checkout);

    let heddle = repo::Repository::open(&checkout).expect("open Heddle metadata");
    let state = heddle
        .refs()
        .get_thread(&objects::object::ThreadName::new("main"))
        .expect("read main thread")
        .expect("main state");
    heddle_git_projection::git_notes::write_note(
        &source,
        first,
        &heddle_git_projection::git_notes::HeddleNote {
            source_state: None,
            state_id: state.to_string(),
            change_id: state.to_string(),
            agent: None,
            confidence: Some(0.8),
            status: "published".to_string(),
            omitted_annotations_breakdown: None,
            signal_counts: None,
            attribution: None,
        },
    )
    .expect("write remote Heddle note");
    let second = write_commit(&source, Some(first), b"two\n", b"two\n");
    publish_branch(&source, "main", Some(first), second);

    let pulled = run(&temp, &checkout, &["--output", "json", "pull"]);
    assert!(
        pulled.status.success(),
        "pull failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pulled.stdout),
        String::from_utf8_lossy(&pulled.stderr)
    );
    let pulled: Value = serde_json::from_slice(&pulled.stdout).expect("pull JSON");
    assert_eq!(pulled["commits_seen_scope"], "branches_and_heddle_notes");
    let local = SleyRepository::discover(&checkout).expect("reopen checkout");
    let note = heddle_git_projection::git_notes::read_note(&local, first)
        .expect("read local note")
        .expect("Heddle note fetched");
    assert_eq!(note.state_id, state.to_string());
    assert!(!checkout.join(".heddle/git").exists());
}
