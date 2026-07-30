// SPDX-License-Identifier: Apache-2.0

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use serde_json::Value;
use sley::{
    CommitObject, EntryKind, GitObjectType, ObjectId, RefPrecondition, ReferenceTarget,
    Repository as SleyRepository,
    plumbing::{sley_object::EncodedObject, sley_refs::ReflogEntry},
};
use tempfile::TempDir;

#[path = "support/git_https.rs"]
mod git_https;

use git_https::PrivateCaGitServer;

struct PullFixture {
    ca_path: PathBuf,
    checkout: PathBuf,
    _server: PrivateCaGitServer,
    source_path: PathBuf,
    temp: TempDir,
}

impl PullFixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("tempdir");
        let source_path = temp.path().join("source.git");
        let source = SleyRepository::init_bare(&source_path).expect("source repository");
        let first = write_commit(&source, None, b"one\n");
        publish_main(&source, None, first);
        source
            .apply_config_edit_plan(
                sley::ConfigEditPlan::new(source.common_dir().join("config")).with_operation(
                    sley::ConfigEdit::set("http.receivepack", "true")
                        .expect("receive-pack config edit"),
                ),
            )
            .expect("enable receive-pack");
        std::fs::write(source_path.join("HEAD"), b"ref: refs/heads/main\n").expect("source HEAD");
        let checkout = temp.path().join("checkout");
        let cloned = heddle(
            &temp,
            temp.path(),
            &["clone", path(&source_path), path(&checkout)],
        );
        assert!(cloned.status.success(), "local clone: {}", stderr(&cloned));

        let server = PrivateCaGitServer::spawn(temp.path());
        let local = SleyRepository::discover(&checkout).expect("checkout repository");
        local
            .apply_config_edit_plan(
                sley::ConfigEditPlan::new(local.common_dir().join("config")).with_operation(
                    sley::ConfigEdit::set("remote.origin.url", server.url())
                        .expect("remote URL edit"),
                ),
            )
            .expect("set HTTPS remote");
        let second = write_commit(&source, Some(first), b"two\n");
        publish_main(&source, Some(first), second);
        let ca_path = temp.path().join("private-ca.pem");
        std::fs::write(&ca_path, server.ca_pem()).expect("write private CA");
        Self {
            ca_path,
            checkout,
            _server: server,
            source_path,
            temp,
        }
    }

    fn pull(&self, ca: bool) -> std::process::Output {
        let mut command = heddle_command(&self.temp, &self.checkout);
        command.args(["--output", "json", "pull"]);
        if ca {
            command.env("HEDDLE_REMOTE_TLS_CA_CERT", &self.ca_path);
        }
        command.output().expect("run Heddle pull")
    }

    fn push_new_commit(&self) -> (std::process::Output, ObjectId) {
        let checkout = SleyRepository::discover(&self.checkout).expect("checkout repository");
        let old = checkout
            .find_reference("refs/heads/main")
            .expect("main lookup")
            .expect("main ref")
            .peeled_oid(&checkout)
            .expect("peel main")
            .expect("main OID");
        let new = write_commit(&checkout, Some(old), b"three\n");
        publish_main(&checkout, Some(old), new);

        let mut command = heddle_command(&self.temp, &self.checkout);
        command
            .args(["--output", "json", "push"])
            .env("HEDDLE_REMOTE_TLS_CA_CERT", &self.ca_path);
        (command.output().expect("run Heddle push"), new)
    }
}

#[test]
fn git_overlay_pull_honours_remote_tls_ca_cert() {
    let fixture = PullFixture::new();
    let pulled = fixture.pull(true);
    assert!(
        pulled.status.success(),
        "private-CA pull failed: {}",
        stderr(&pulled)
    );
    let output: Value = serde_json::from_slice(&pulled.stdout).expect("pull JSON");
    assert_eq!(output["transport"], "git");
    assert_eq!(
        std::fs::read_to_string(fixture.checkout.join("tracked.txt")).expect("checkout file"),
        "two\n"
    );
}

#[test]
fn git_overlay_tls_failure_names_remote_ca_configuration() {
    let fixture = PullFixture::new();
    let pulled = fixture.pull(false);
    assert!(!pulled.status.success(), "untrusted private CA must fail");
    assert!(
        stderr(&pulled).contains("HEDDLE_REMOTE_TLS_CA_CERT"),
        "TLS failure must name the CA configuration: {}",
        stderr(&pulled)
    );
}

#[test]
fn git_overlay_push_honours_remote_tls_ca_cert() {
    let fixture = PullFixture::new();
    let pulled = fixture.pull(true);
    assert!(pulled.status.success(), "fixture pull: {}", stderr(&pulled));
    let (pushed, expected) = fixture.push_new_commit();
    assert!(
        pushed.status.success(),
        "private-CA push failed: {}",
        stderr(&pushed)
    );
    let source = SleyRepository::open(&fixture.source_path).expect("source repository");
    let actual = source
        .find_reference("refs/heads/main")
        .expect("main lookup")
        .expect("main ref")
        .peeled_oid(&source)
        .expect("peel main");
    assert_eq!(actual, Some(expected));
}

fn write_commit(repo: &SleyRepository, parent: Option<ObjectId>, content: &[u8]) -> ObjectId {
    let blob = repo.write_blob(content).expect("blob");
    let empty = repo
        .write_tree(sley::TreeEditor::new())
        .expect("empty tree");
    let mut tree = repo.edit_tree(&empty).expect("edit tree");
    tree.upsert("tracked.txt", EntryKind::Blob, blob);
    let tree = repo.write_tree(tree).expect("tree");
    let identity = b"Heddle Test <heddle@example.com> 0 +0000".to_vec();
    let commit = CommitObject {
        tree,
        parents: parent.into_iter().collect(),
        author: identity.clone(),
        committer: identity,
        encoding: None,
        message: content.to_vec(),
    };
    repo.write_object(EncodedObject::new(GitObjectType::Commit, commit.write()))
        .expect("commit")
}

fn publish_main(repo: &SleyRepository, old: Option<ObjectId>, new: ObjectId) {
    let references = repo.references();
    let mut transaction = references.transaction();
    transaction.update_to(
        "refs/heads/main",
        ReferenceTarget::Direct(new),
        RefPrecondition::Any,
        Some(ReflogEntry {
            old_oid: old.unwrap_or_else(|| ObjectId::null(repo.object_format())),
            new_oid: new,
            committer: b"Heddle Test <heddle@example.com> 0 +0000".to_vec(),
            message: b"update main".to_vec(),
        }),
    );
    transaction.commit().expect("publish main");
}

fn heddle(temp: &TempDir, cwd: &Path, args: &[&str]) -> std::process::Output {
    heddle_command(temp, cwd)
        .args(args)
        .output()
        .expect("run Heddle")
}

fn heddle_command(temp: &TempDir, cwd: &Path) -> Command {
    let config = temp.path().join("config.toml");
    std::fs::write(
        &config,
        "[principal]\nname = \"Heddle Test\"\nemail = \"heddle@example.com\"\n",
    )
    .expect("write Heddle config");
    let mut command = Command::new(env!("CARGO_BIN_EXE_heddle"));
    command
        .current_dir(cwd)
        .env("PATH", "")
        .env("HOME", temp.path())
        .env("HEDDLE_CONFIG", config)
        .env("NO_COLOR", "1")
        .env_remove("HEDDLE_REMOTE_TLS_CA_CERT")
        .env_remove("SSL_CERT_FILE")
        .env_remove("SSL_CERT_DIR");
    command
}

fn path(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
