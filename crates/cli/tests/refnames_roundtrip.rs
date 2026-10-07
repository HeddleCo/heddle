// SPDX-License-Identifier: Apache-2.0
//! Git import identities survive the native storage and local fetch path.
use std::{path::Path, process::Command};

use cli::{ObjectStore, Repository};
use hosted_client::client::LocalSync;
use objects::{name_encoding::git_name, object::ThreadName};
use refs::Head;
use tempfile::TempDir;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Refname Test")
        .env("GIT_AUTHOR_EMAIL", "refnames@heddle.test")
        .env("GIT_COMMITTER_NAME", "Refname Test")
        .env("GIT_COMMITTER_EMAIL", "refnames@heddle.test")
        .output()
        .expect("run Git");
    assert!(
        output.status.success(),
        "Git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("Git UTF-8 output")
}

fn round_trip(name: &str) {
    let source = TempDir::new().expect("Git source");
    let native = TempDir::new().expect("native source");
    let destination = TempDir::new().expect("fetch destination");
    git(source.path(), &["init", "-b", "main"]);
    std::fs::write(source.path().join("payload.txt"), "exact branch identity\n").expect("payload");
    git(source.path(), &["add", "."]);
    git(source.path(), &["commit", "-m", "fixture"]);
    let oid = git(source.path(), &["rev-parse", "HEAD"]);
    let oid = oid.trim_end_matches('\n');
    let full = format!("refs/heads/{name}");
    if name.len() > 240 {
        std::fs::write(
            source.path().join(".git/packed-refs"),
            format!("{oid} {full}\n"),
        )
        .expect("packed long Git ref");
    } else {
        git(source.path(), &["update-ref", &full, oid]);
    }
    ingest::import_git_into(source.path(), native.path()).expect("all-branch import");
    let source_repo = Repository::open(native.path()).expect("native repository");
    let thread = ThreadName::from_git_branch(name).expect("Git boundary mapping");
    assert_eq!(git_name(&thread), name);
    let state = source_repo
        .refs()
        .get_thread(&thread)
        .expect("stored ref")
        .expect("imported tip");
    let head = Head::Attached {
        thread: thread.clone(),
    };
    source_repo
        .refs()
        .write_head(&head)
        .expect("attach native HEAD");
    assert_eq!(source_repo.refs().read_head().expect("exact HEAD"), head);
    source_repo.refs().pack_refs().expect("native packed refs");
    drop(source_repo);
    let sync = LocalSync::open(native.path()).expect("reopen for fetch");
    assert!(
        sync.list_threads()
            .expect("advertised listing")
            .contains(&(thread.to_string(), state))
    );
    let target = Repository::init(destination.path()).expect("fresh receiver");
    assert!(
        sync.fetch_state(&target, &state)
            .expect("fetch dependency closure")
            > 0
    );
    target
        .refs()
        .set_thread(&thread, &state)
        .expect("install fetched ref");
    assert_eq!(
        target.refs().get_thread(&thread).expect("receiver ref"),
        Some(state)
    );
    assert!(
        target
            .store()
            .get_state(&state)
            .expect("fetched state")
            .is_some()
    );
    let tree = target
        .store()
        .get_state(&state)
        .expect("state read")
        .expect("state")
        .tree;
    assert!(
        target
            .store()
            .get_tree(&tree)
            .expect("fetched tree")
            .is_some()
    );
}

#[test]
fn import_storage_head_pack_list_fetch_equals() {
    round_trip("feat/mcp=timeout");
}
#[test]
fn import_storage_head_pack_list_fetch_comma() {
    round_trip("a,b");
}
#[test]
fn import_storage_head_pack_list_fetch_unicode() {
    round_trip("ünicode/ブランチ");
}
#[test]
fn import_storage_head_pack_list_fetch_at() {
    round_trip("@");
}
#[test]
fn import_storage_head_pack_list_fetch_plus() {
    round_trip("x+y");
}
#[test]
fn import_storage_head_pack_list_fetch_nbsp() {
    round_trip("trailing\u{a0}");
}
#[test]
fn import_storage_head_pack_list_fetch_replacement() {
    round_trip("literal\u{fffd}");
}
#[test]
fn import_storage_head_pack_list_fetch_long() {
    round_trip(&"界".repeat(337));
}
#[test]
fn import_storage_head_pack_list_fetch_reserved() {
    round_trip("heddle/foo");
    round_trip(&format!("heddle/{}", "界".repeat(333)));
}
