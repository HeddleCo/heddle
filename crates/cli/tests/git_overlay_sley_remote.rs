// SPDX-License-Identifier: Apache-2.0

use std::{path::Path, process::Command};

use serde_json::Value;
use sley::{
    CommitObject, EntryKind, GitObjectType, ObjectId, RefPrecondition, ReferenceTarget,
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
    let identity = b"Heddle Test <heddle@example.com> 0 +0000".to_vec();
    let references = repo.references();
    let mut refs = references.transaction();
    refs.update_to(
        format!("refs/heads/{branch}"),
        ReferenceTarget::Direct(new),
        RefPrecondition::Any,
        Some(ReflogEntry {
            old_oid: old.unwrap_or_else(|| ObjectId::null(repo.object_format())),
            new_oid: new,
            committer: identity,
            message: format!("update {branch}").into_bytes(),
        }),
    );
    refs.commit().expect("publish branch");
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
