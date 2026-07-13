// SPDX-License-Identifier: Apache-2.0

use std::process::Command;

use serde_json::Value;
use sley::{
    CommitObject, EntryKind, GitObjectType, ObjectId, RefPrecondition, ReferenceTarget,
    Repository as SleyRepository,
    plumbing::{sley_object::EncodedObject, sley_refs::ReflogEntry},
};
use tempfile::TempDir;

fn seed_git_source(path: &std::path::Path) {
    let repo = SleyRepository::init_bare(path).expect("initialize Git source with Sley");
    let blob = repo
        .write_blob(b"cloned without git\n")
        .expect("write blob");
    let empty_tree = repo
        .write_tree(sley::TreeEditor::new())
        .expect("write empty tree");
    let mut tree = repo.edit_tree(&empty_tree).expect("edit tree");
    tree.upsert("tracked.txt", EntryKind::Blob, blob);
    let tree = repo.write_tree(tree).expect("write populated tree");
    let identity = b"Heddle Test <heddle@example.com> 0 +0000".to_vec();
    let commit = CommitObject {
        tree,
        parents: Vec::new(),
        author: identity.clone(),
        committer: identity.clone(),
        encoding: None,
        message: b"seed\n".to_vec(),
    };
    let commit = repo
        .write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("write commit");
    let references = repo.references();
    let mut refs = references.transaction();
    refs.update_to(
        "refs/heads/main".to_string(),
        ReferenceTarget::Direct(commit),
        RefPrecondition::Any,
        Some(ReflogEntry {
            old_oid: ObjectId::null(repo.object_format()),
            new_oid: commit,
            committer: identity,
            message: b"seed main".to_vec(),
        }),
    );
    refs.commit().expect("publish main");
    std::fs::write(path.join("HEAD"), b"ref: refs/heads/main\n").expect("write source HEAD");
}

#[test]
fn clone_git_source_succeeds_with_empty_process_path() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("source.git");
    let destination = temp.path().join("checkout");
    seed_git_source(&source);

    let config = temp.path().join("heddle-config.toml");
    std::fs::write(
        &config,
        "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n",
    )
    .expect("write Heddle config");
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "--output",
            "json",
            "clone",
            source.to_str().expect("source UTF-8"),
            destination.to_str().expect("destination UTF-8"),
        ])
        .current_dir(temp.path())
        .env("PATH", "")
        .env("HOME", temp.path())
        .env("HEDDLE_CONFIG", &config)
        .env("NO_COLOR", "1")
        .output()
        .expect("run Heddle directly without PATH lookup");

    assert!(
        output.status.success(),
        "clone must not require the git executable\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("clone JSON");
    assert_eq!(result["output_kind"], "clone");
    assert_eq!(result["transport"], "git");
    assert_eq!(result["repository_capability"], "git-overlay");
    assert_eq!(result["branch"], "main");
    assert!(destination.join(".git").is_dir());
    assert!(destination.join(".heddle").is_dir());
    assert!(
        !destination.join(".heddle/git").exists(),
        "Git Overlay clone must not create a second Git object database"
    );
    assert_eq!(
        std::fs::read_to_string(destination.join("tracked.txt")).expect("materialized file"),
        "cloned without git\n"
    );
    assert!(
        !std::fs::read_dir(temp.path())
            .expect("read clone parent")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with(".checkout.heddle-clone-")),
        "successful clone must not leave its staging directory behind"
    );
}

#[test]
fn git_overlay_insecure_refusal_leaves_destination_absent() {
    let temp = TempDir::new().expect("tempdir");
    let source = temp.path().join("source.git");
    let destination = temp.path().join("checkout");
    seed_git_source(&source);

    let config = temp.path().join("heddle-config.toml");
    std::fs::write(
        &config,
        "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n",
    )
    .expect("write Heddle config");
    let output = Command::new(env!("CARGO_BIN_EXE_heddle"))
        .args([
            "clone",
            source.to_str().expect("source UTF-8"),
            destination.to_str().expect("destination UTF-8"),
            "--insecure",
        ])
        .current_dir(temp.path())
        .env("PATH", "")
        .env("HOME", temp.path())
        .env("HEDDLE_CONFIG", &config)
        .env("NO_COLOR", "1")
        .output()
        .expect("run Heddle directly without PATH lookup");

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--insecure is not supported for Git-overlay clones")
    );
    assert!(!destination.exists());
}
