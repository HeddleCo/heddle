// SPDX-License-Identifier: Apache-2.0
//! Repository trust at discovery (heddle#2034): a `.heddle` that is content of
//! an enclosing worktree, or that another user owns, is never selected
//! implicitly. Heddle vouches for the nested repositories it creates itself.

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

use super::discovery::{TrustedOwners, ensure_repository_trusted_with};
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

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} in {}", dir.display());
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

/// Copy a hostile repository's metadata to `at/.heddle`, the way a checkout
/// of tracked content would write it.
fn plant_hostile_repo(scratch: &Path, at: &Path) -> PathBuf {
    copy_tree(&hostile_heddle_dir(scratch), &at.join(".heddle"));
    fs::write(at.join("README.md"), "fixture\n").unwrap();
    fs::write(at.join("attacker-ca.pem"), "not a real CA\n").unwrap();
    at.canonicalize().unwrap()
}

/// Enclosing repository at `<tmp>/outer` with a checked-out hostile
/// repository planted at `outer/sub/.heddle`.
fn enclosing_with_planted_repo() -> (TempDir, Repository, PathBuf) {
    let temp = TempDir::new().unwrap();
    let enclosing = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let planted = plant_hostile_repo(temp.path(), &enclosing.root().join("sub"));
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

fn unreachable_none() -> HeddleError {
    HeddleError::Config("open_existing found no repository candidate".to_string())
}

fn me() -> u32 {
    #[cfg(unix)]
    {
        crate::daemon::peer::current_euid()
    }
    #[cfg(not(unix))]
    {
        1000
    }
}

fn owned_by_me(_path: &Path, _metadata: &fs::Metadata) -> u32 {
    me()
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
    assert!(matches!(
        crate::discover_repository_config(&deeper),
        Err(HeddleError::UntrustedRepository { .. })
    ));
}

/// P1: a planted `.git` directory next to the planted `.heddle` (a real Git
/// repository whose `info/exclude` even carries Heddle's sidecar line) is
/// content, not a nested-repository boundary.
#[test]
fn planted_git_dir_beside_planted_heddle_is_still_embedded() {
    let (_temp, enclosing, planted) = enclosing_with_planted_repo();
    git(&planted, &["init", "--quiet"]);
    fs::write(planted.join(".git/info/exclude"), "/.heddle/\n").unwrap();
    expect_embedded(Repository::open(&planted), &planted, enclosing.root());
}

/// P2-a: a Git clone (gitfile layout, as a submodule has) whose upstream
/// committed `.heddle/` delivers that metadata as tracked content.
#[test]
fn git_delivered_tracked_heddle_in_nested_clone_is_embedded() {
    let temp = TempDir::new().unwrap();
    let enclosing = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let upstream = temp.path().join("upstream");
    fs::create_dir_all(&upstream).unwrap();
    plant_hostile_repo(temp.path(), &upstream);
    git(&upstream, &["init", "--quiet", "-b", "main"]);
    git(&upstream, &["add", "-f", "."]);
    git(&upstream, &["commit", "--quiet", "-m", "ship .heddle"]);

    let vendor = enclosing.root().join("vendor");
    fs::create_dir_all(&vendor).unwrap();
    let gitdir = temp.path().join("modules/x");
    fs::create_dir_all(temp.path().join("modules")).unwrap();
    git(
        &vendor,
        &[
            "clone",
            "--quiet",
            "--separate-git-dir",
            gitdir.to_str().unwrap(),
            upstream.to_str().unwrap(),
            "x",
        ],
    );
    let nested = vendor.join("x").canonicalize().unwrap();
    assert!(nested.join(".git").is_file(), "clone must use a gitfile");
    assert!(
        nested.join(".heddle/HEAD").is_file(),
        ".heddle must be tracked"
    );
    expect_embedded(Repository::open(&nested), &nested, enclosing.root());
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
fn heddle_created_nested_repository_is_trusted_automatically() {
    let temp = TempDir::new().unwrap();
    let outer = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let nested = outer.root().join("libs/inner");
    Repository::init_default(&nested).unwrap();
    let opened = Repository::open(nested.join(".")).unwrap();
    assert_eq!(opened.root(), nested.canonicalize().unwrap());
}

/// The creation record binds the nested `.heddle`'s identity: content later
/// checked out at the same path is not covered by it.
#[test]
fn creation_record_does_not_cover_a_replacement_at_the_same_path() {
    let temp = TempDir::new().unwrap();
    let outer = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let nested = outer.root().join("sub");
    Repository::init_default(&nested).unwrap();
    fs::remove_dir_all(nested.join(".heddle")).unwrap();
    let planted = plant_hostile_repo(temp.path(), &nested);
    expect_embedded(Repository::open(&planted), &planted, outer.root());
}

#[test]
fn trust_list_ownership_and_sudo_decide_untrusted_roots() {
    let (_temp, enclosing, planted) = enclosing_with_planted_repo();
    assert!(ensure_repository_trusted_with(&planted, &[], None, &owned_by_me).is_err());
    assert!(
        ensure_repository_trusted_with(
            &planted,
            std::slice::from_ref(&planted),
            None,
            &owned_by_me
        )
        .is_ok()
    );

    let me = me();
    let other = me.wrapping_add(1);
    let outer = enclosing.root().canonicalize().unwrap();
    let as_me = TrustedOwners {
        euid: me,
        sudo_uid: None,
    };
    let as_other = TrustedOwners {
        euid: other,
        sudo_uid: None,
    };
    let sudo_for_me = TrustedOwners {
        euid: other,
        sudo_uid: Some(me),
    };
    assert!(ensure_repository_trusted_with(&outer, &[], Some(as_me), &owned_by_me).is_ok());
    match ensure_repository_trusted_with(&outer, &[], Some(as_other), &owned_by_me) {
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
        ensure_repository_trusted_with(&outer, &[], Some(sudo_for_me), &owned_by_me).is_ok(),
        "root under sudo trusts the invoking user's repository (SUDO_UID)"
    );
    assert!(
        ensure_repository_trusted_with(
            &outer,
            std::slice::from_ref(&outer),
            Some(as_other),
            &owned_by_me
        )
        .is_ok(),
        "an explicitly trusted root overrides ownership"
    );
}

/// A `.heddle` symlink planted in a directory another user owns, pointing at
/// this user's own repository metadata, is refused: the worktree it would
/// capture is theirs.
#[cfg(unix)]
#[test]
fn symlinked_heddle_in_foreign_owned_directory_is_refused() {
    let temp = TempDir::new().unwrap();
    let mine = crate::init_test_repository(temp.path().join("mine")).unwrap();
    let theirs = temp.path().join("theirs");
    fs::create_dir_all(&theirs).unwrap();
    std::os::unix::fs::symlink(mine.heddle_dir(), theirs.join(".heddle")).unwrap();
    let theirs = theirs.canonicalize().unwrap();
    let me = me();
    let other = me.wrapping_add(1);
    let owners = TrustedOwners {
        euid: me,
        sudo_uid: None,
    };
    let theirs_for_check = theirs.clone();
    let owner_of = move |path: &Path, _metadata: &fs::Metadata| {
        if path == theirs_for_check { other } else { me }
    };
    match ensure_repository_trusted_with(&theirs, &[], Some(owners), &owner_of) {
        Err(HeddleError::UntrustedRepository {
            reason: UntrustedRepositoryReason::ForeignOwner { path, owner, .. },
            ..
        }) => {
            assert_eq!(path, theirs);
            assert_eq!(owner, other);
        }
        other => panic!("expected a foreign-owned symlink parent refusal, got {other:?}"),
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
fn linked_checkout_created_inside_worktree_opens() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let checkout = repo.root().join("wt");
    Repository::init_worktree(&checkout, repo.heddle_dir()).unwrap();
    let opened = Repository::open(&checkout).unwrap();
    assert_eq!(opened.root(), checkout.as_path());
}

/// A planted checkout pointer is refused even when it names the enclosing
/// repository's own store: only Heddle's creation record vouches for it.
#[test]
fn planted_checkout_pointer_is_refused_whatever_store_it_names() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let elsewhere = crate::init_test_repository(temp.path().join("elsewhere")).unwrap();
    for (name, store) in [
        ("own", repo.heddle_dir()),
        ("redirect", elsewhere.heddle_dir()),
    ] {
        let staging = temp.path().join(format!("staging-{name}"));
        Repository::init_worktree(&staging, store).unwrap();
        let planted = repo.root().join(name);
        copy_tree(&staging.join(".heddle"), &planted.join(".heddle"));
        let planted = planted.canonicalize().unwrap();
        expect_embedded(Repository::open(&planted), &planted, repo.root());
    }
}

#[test]
fn heddle_created_nested_git_sidecar_is_trusted() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let nested_git = repo.root().join("vendor");
    fs::create_dir_all(&nested_git).unwrap();
    git(&nested_git, &["init", "--quiet"]);
    // Opening inside the nested Git repository bootstraps its own sidecar and
    // reopens there; Heddle created it, so it vouches for it.
    let opened = Repository::open(&nested_git).unwrap();
    assert_eq!(opened.root(), nested_git.canonicalize().unwrap());
}

/// P2-b: config readers get `open`'s mount guard, and a checkout's config is
/// its store's.
#[test]
fn discover_repository_config_matches_open_admission() {
    let temp = TempDir::new().unwrap();
    let repo = crate::init_test_repository(temp.path().join("outer")).unwrap();
    let main_config = repo
        .heddle_dir()
        .canonicalize()
        .unwrap()
        .join("config.toml");
    assert_eq!(
        crate::discover_repository_config(repo.root()).unwrap(),
        Some(main_config.clone())
    );

    let checkout = temp.path().join("checkout");
    Repository::init_worktree(&checkout, repo.heddle_dir()).unwrap();
    assert_eq!(
        crate::discover_repository_config(&checkout).unwrap(),
        Some(main_config),
        "a checkout's config is its store's"
    );

    let mount_root = repo.managed_checkout_path("virt");
    fs::create_dir_all(&mount_root).unwrap();
    assert!(
        matches!(
            crate::discover_repository_config(&mount_root),
            Err(HeddleError::Config(message)) if message.contains("virtualized thread mount")
        ),
        "the parent's config must not be read from inside a metadata-less mount"
    );
}
