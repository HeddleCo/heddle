// SPDX-License-Identifier: Apache-2.0
//! Repository trust at discovery (heddle#2034): a `.heddle` that is content of
//! an enclosing worktree, or that another user owns, is never selected
//! implicitly.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use objects::{
    error::UntrustedRepositoryReason,
    object::{LeafPolicy, resolve_tree_path},
};
use tempfile::TempDir;

use super::discovery::ensure_repository_trusted_with;
use crate::{HeddleError, Repository};

const ATTACKER_UPSTREAM: &str = "https://attacker.example.test";

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// A complete, openable repository whose config redirects TLS trust and the
/// hosted upstream, and which ships a pre-push hook.
fn hostile_heddle_dir(scratch: &Path) -> PathBuf {
    let source = scratch.join("hostile-source");
    let repo = crate::init_test_repository(&source).unwrap();
    let mut config = repo.config().clone();
    config.hosted.upstream_url = Some(ATTACKER_UPSTREAM.to_string());
    config.remote.tls_ca_certificate_path = Some(PathBuf::from("attacker-ca.pem"));
    config.save(&repo.heddle_dir().join("config.toml")).unwrap();
    fs::create_dir_all(repo.heddle_dir().join("hooks")).unwrap();
    fs::write(
        repo.heddle_dir().join("hooks/pre-push"),
        "#!/bin/sh\necho pwned\n",
    )
    .unwrap();
    repo.heddle_dir().to_path_buf()
}

/// Enclosing repository at `<tmp>/outer` with a checked-out hostile
/// repository planted at `outer/sub/.heddle`.
fn enclosing_with_planted_repo() -> (TempDir, Repository, PathBuf) {
    let temp = TempDir::new().unwrap();
    let outer = temp.path().join("outer");
    let enclosing = crate::init_test_repository(&outer).unwrap();
    let planted = outer.join("sub");
    copy_tree(&hostile_heddle_dir(temp.path()), &planted.join(".heddle"));
    fs::write(planted.join("README.md"), "fixture\n").unwrap();
    fs::write(planted.join("attacker-ca.pem"), "not a real CA\n").unwrap();
    let planted = planted.canonicalize().unwrap();
    (temp, enclosing, planted)
}

fn expect_embedded(result: crate::Result<Repository>, planted: &Path, enclosing: &Path) {
    match result {
        Err(HeddleError::UntrustedRepository { root, reason }) => {
            assert_eq!(root, planted);
            assert_eq!(
                reason,
                UntrustedRepositoryReason::Embedded {
                    enclosing: enclosing.canonicalize().unwrap()
                }
            );
        }
        Err(other) => panic!("expected an embedded-repository refusal, got {other}"),
        Ok(repo) => panic!(
            "opened the planted repository at {} (upstream {:?})",
            repo.root().display(),
            repo.config().hosted.upstream_url
        ),
    }
}

#[test]
fn planted_nested_repository_is_not_discovered_from_inside_it() {
    let (_temp, enclosing, planted) = enclosing_with_planted_repo();
    let deeper = planted.join("src");
    fs::create_dir_all(&deeper).unwrap();

    expect_embedded(Repository::open(&planted), &planted, enclosing.root());
    expect_embedded(Repository::open(&deeper), &planted, enclosing.root());
    expect_embedded(
        Repository::open_existing(&deeper).and_then(|repo| repo.ok_or_else(unreachable_none)),
        &planted,
        enclosing.root(),
    );
}

fn unreachable_none() -> HeddleError {
    HeddleError::Config("open_existing found no repository candidate".to_string())
}

#[test]
fn explicitly_trusted_nested_repository_opens() {
    let (_temp, _enclosing, planted) = enclosing_with_planted_repo();
    // The process-wide list is first-registration-wins; this is the only test
    // that registers one, and it names a path private to this test.
    super::discovery::set_safe_repositories([planted.clone()]);

    let opened = Repository::open(&planted).unwrap();
    assert_eq!(opened.root(), planted);
    assert_eq!(
        opened.config().hosted.upstream_url.as_deref(),
        Some(ATTACKER_UPSTREAM),
        "explicit trust honours the repository's own config"
    );
}

#[test]
fn trust_list_and_ownership_decide_untrusted_roots() {
    let (_temp, enclosing, planted) = enclosing_with_planted_repo();
    assert!(ensure_repository_trusted_with(&planted, &[], None).is_err());
    assert!(ensure_repository_trusted_with(&planted, std::slice::from_ref(&planted), None).is_ok());

    #[cfg(unix)]
    {
        let me = crate::daemon::peer::current_euid();
        let other = me.wrapping_add(1);
        let outer = enclosing.root().canonicalize().unwrap();
        assert!(ensure_repository_trusted_with(&outer, &[], Some(me)).is_ok());
        match ensure_repository_trusted_with(&outer, &[], Some(other)) {
            Err(HeddleError::UntrustedRepository {
                reason: UntrustedRepositoryReason::ForeignOwner { owner, current, .. },
                ..
            }) => {
                assert_eq!(owner, me);
                assert_eq!(current, other);
            }
            other => panic!("expected a foreign-owner refusal, got {other:?}"),
        }
        assert!(
            ensure_repository_trusted_with(&outer, std::slice::from_ref(&outer), Some(other))
                .is_ok(),
            "an explicitly trusted root overrides ownership"
        );
    }
}

#[test]
fn standalone_repository_is_unaffected() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("standalone")).unwrap();
    let nested = repo.root().join("a/b");
    fs::create_dir_all(&nested).unwrap();
    let opened = Repository::open(&nested).unwrap();
    assert_eq!(
        opened.root().canonicalize().unwrap(),
        repo.root().canonicalize().unwrap()
    );
}

#[test]
fn enclosing_repository_captures_nested_fixture_as_content() {
    let (_temp, enclosing, _planted) = enclosing_with_planted_repo();
    let state = enclosing
        .snapshot(Some("capture fixture".to_string()), None)
        .unwrap();
    for path in [
        "sub/.heddle/HEAD",
        "sub/.heddle/config.toml",
        "sub/.heddle/hooks/pre-push",
        "sub/README.md",
    ] {
        assert!(
            resolve_tree_path(
                enclosing.store(),
                &state.tree,
                Path::new(path),
                LeafPolicy::Entry
            )
            .unwrap()
            .is_some(),
            "nested fixture path {path} must be captured as content"
        );
    }
}

#[test]
fn linked_checkout_of_enclosing_store_inside_its_worktree_opens() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let checkout = repo.root().join("wt");
    Repository::init_worktree(&checkout, repo.heddle_dir()).unwrap();
    let opened = Repository::open(&checkout).unwrap();
    assert_eq!(opened.root(), checkout.as_path());
}

#[test]
fn nested_checkout_pointing_at_another_store_is_refused() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let elsewhere = crate::init_test_repository(temp.path().join("elsewhere")).unwrap();
    let planted = repo.root().join("redirect");
    Repository::init_worktree(&planted, elsewhere.heddle_dir()).unwrap();
    expect_embedded(
        Repository::open(&planted),
        &planted.canonicalize().unwrap(),
        repo.root(),
    );
}

#[test]
fn nested_git_repository_sidecar_is_not_embedded() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let nested_git = repo.root().join("vendor");
    fs::create_dir_all(&nested_git).unwrap();
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&nested_git)
        .status()
        .unwrap();
    assert!(status.success());
    // Opening inside the nested Git repository bootstraps its own sidecar and
    // reopens there; that sidecar must not be refused as embedded content.
    let opened = Repository::open(&nested_git).unwrap();
    assert_eq!(opened.root(), nested_git.canonicalize().unwrap());
}
