// SPDX-License-Identifier: Apache-2.0
//! Independent current-authority checks across the complete prepared history.
//! Synthetic local signatures prove bytes; the supplied verifier models a fresh
//! receiver authority decision and may revoke an otherwise valid historical key.

use crypto::{Ed25519Signer, Signer, thread_operation::SignedOperation};
use heddle_git_projection::{
    GitProjectionError,
    gateway_view::{
        HistoryTip, ViewLimits, export_public_git_history, export_public_git_history_with_authority,
    },
    gateway_write::{
        LocalPush, PreparedGitPush, PushAuthor, PushUpdate, WriteLimits, prepare_git_push,
    },
};
use objects::object::{
    Attribution, ContentHash, Principal, StateId, thread_replication::SourceAuthor,
};
use repo::Repository;
use sley::{GitObjectType, ObjectId, Repository as GitRepository};
use std::{cell::RefCell, collections::BTreeSet};

struct Fixture {
    _directory: tempfile::TempDir,
    native: Repository,
    git: GitRepository,
    base: StateId,
    old: ObjectId,
    new: ObjectId,
    signer: Ed25519Signer,
}

fn fixture() -> Fixture {
    let directory = tempfile::tempdir().expect("fixture directory");
    let native = Repository::init_default(directory.path().join("native")).expect("native");
    let mut base = None;
    for index in 0..2 {
        std::fs::write(
            native.root().join("public.txt"),
            format!("public revision {index}\n"),
        )
        .expect("public file");
        base = Some(
            native
                .snapshot_with_attribution(
                    Some(format!("public revision {index}")),
                    None,
                    Attribution::human(Principal::new("Synthetic owner", "owner@example.invalid")),
                )
                .expect("native capture")
                .state_id,
        );
    }
    let base = base.expect("two captures");
    let git =
        GitRepository::init_bare(directory.path().join("quarantine.git")).expect("quarantine");
    let mapping = export_public_git_history(
        &native,
        &git,
        &[HistoryTip {
            thread: "main",
            state: base,
        }],
        &["main"],
        ViewLimits::default(),
    )
    .expect("current public base");
    let old = mapping.get_git(&base).expect("projected base");
    let blob = git
        .write_raw_object(GitObjectType::Blob, b"new public bytes\n".to_vec())
        .expect("blob");
    let mut tree = b"100644 public.txt\0".to_vec();
    tree.extend_from_slice(blob.as_bytes());
    let tree = git
        .write_raw_object(GitObjectType::Tree, tree)
        .expect("tree");
    let new = git.write_raw_object(GitObjectType::Commit, format!(
        "tree {tree}\nparent {old}\nauthor Forged Administrator <admin@example.invalid> 1700000000 +0000\ncommitter Forged Administrator <admin@example.invalid> 1700000000 +0000\n\nordinary untrusted Git metadata\n"
    ).into_bytes()).expect("new Git commit");
    let signer = native
        .native_thread_signer(&native.native_thread("main").expect("main"))
        .expect("existing signer");
    Fixture {
        _directory: directory,
        native,
        git,
        base,
        old,
        new,
        signer,
    }
}

fn prepare(f: &Fixture) -> PreparedGitPush {
    let update = PushUpdate {
        thread: "main".into(),
        old: f.old,
        new: f.new,
    };
    prepare_git_push(
        &f.native,
        &f.git,
        LocalPush {
            update: &update,
            expected_native: f.base,
            policy_generation: "current-authority-fixture",
        },
        PushAuthor {
            actor: "verified-actor",
            publisher: f.signer.public_key().try_into().expect("public key"),
            source_author: &SourceAuthor::LocalKey,
        },
        WriteLimits::default(),
        |_| Ok(()),
    )
    .expect("prepare valid immutable history")
}

fn signed(plan: &PreparedGitPush, f: &Fixture) -> Vec<SignedOperation> {
    plan.operations()
        .iter()
        .map(|operation| SignedOperation::sign(operation, &f.signer).expect("sign exact operation"))
        .collect()
}

fn unchanged(f: &Fixture) {
    assert_eq!(
        f.native
            .native_thread("main")
            .expect("main")
            .projection()
            .expect("projection")
            .source_heads,
        vec![f.base]
    );
}

#[test]
fn retained_original_revocation_blocks_new_push_even_when_every_signature_is_valid() {
    let f = fixture();
    let plan = prepare(&f);
    assert_eq!(
        plan.historical_originals().len(),
        2,
        "fixture has retained authority to check"
    );
    let revoked = plan.historical_originals()[0]
        .verify()
        .expect("valid retained signature")
        .id()
        .expect("original ID");
    let signatures = signed(&plan, &f);
    let error = plan
        .bind_signed(
            &f.native,
            signatures,
            |_| Ok(()),
            |replica, operation| {
                replica
                    .verify_local_source_owner(operation)
                    .map_err(|error| GitProjectionError::Git(error.to_string()))?;
                if operation.id()? == revoked {
                    return Err(GitProjectionError::Git(
                        "retained original current authority revoked".into(),
                    ));
                }
                Ok(())
            },
        )
        .err()
        .expect("valid new tip cannot hide a revoked retained original");
    assert!(
        error
            .to_string()
            .contains("retained original current authority revoked"),
        "{error}"
    );
    unchanged(&f);
}

#[test]
fn verifier_observes_every_retained_and_new_original_before_signature_binding_succeeds() {
    let f = fixture();
    let plan = prepare(&f);
    let expected: BTreeSet<ContentHash> = plan
        .historical_originals()
        .iter()
        .map(|original| original.verify().expect("valid original").id().expect("ID"))
        .chain(
            plan.operations()
                .iter()
                .map(|operation| operation.id().expect("ID")),
        )
        .collect();
    assert_eq!(expected.len(), 3);
    let seen = RefCell::new(BTreeSet::new());
    let signatures = signed(&plan, &f);
    plan.bind_signed(
        &f.native,
        signatures,
        |_| Ok(()),
        |replica, operation| {
            replica
                .verify_local_source_owner(operation)
                .map_err(|error| GitProjectionError::Git(error.to_string()))?;
            seen.borrow_mut().insert(operation.id()?);
            Ok(())
        },
    )
    .expect("all originals currently authorized");
    assert_eq!(
        seen.into_inner(),
        expected,
        "Accepted admission and valid signatures do not replace fresh retained-author checks"
    );
    unchanged(&f);
}

#[test]
fn earlier_successful_binding_does_not_cache_retained_authority_on_retry() {
    let f = fixture();
    let first = prepare(&f);
    let signatures = signed(&first, &f);
    first
        .bind_signed(
            &f.native,
            signatures,
            |_| Ok(()),
            |replica, operation| {
                replica
                    .verify_local_source_owner(operation)
                    .map_err(|error| GitProjectionError::Git(error.to_string()))
            },
        )
        .expect("initial current authority");
    let retry = prepare(&f);
    let retained: BTreeSet<_> = retry
        .historical_originals()
        .iter()
        .map(|original| original.verify().expect("original").id().expect("ID"))
        .collect();
    let signatures = signed(&retry, &f);
    let result = retry.bind_signed(
        &f.native,
        signatures,
        |_| Ok(()),
        |_, operation| {
            if retained.contains(&operation.id()?) {
                return Err(GitProjectionError::Git(
                    "historical account removed since prior attempt".into(),
                ));
            }
            Ok(())
        },
    );
    assert!(
        result.is_err(),
        "retry must not borrow the earlier current-authority decision"
    );
    unchanged(&f);
}

#[test]
fn git_author_and_committer_do_not_supply_the_verified_source_actor() {
    let f = fixture();
    let plan = prepare(&f);
    assert_eq!(plan.scope().actor, "verified-actor");
    assert_eq!(plan.receipt().actor, "verified-actor");
    for operation in plan.operations() {
        assert_eq!(operation.publisher.as_slice(), f.signer.public_key());
        let objects::object::thread_replication::ThreadOperationBody::Capture(capture) =
            &operation.body
        else {
            panic!("capture")
        };
        assert_eq!(capture.author, SourceAuthor::LocalKey);
    }
    unchanged(&f);
}

fn assert_empty(sink: &GitRepository) {
    let output = std::process::Command::new("git")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("--git-dir")
        .arg(sink.git_dir())
        .args(["cat-file", "--batch-all-objects", "--batch-check"])
        .output()
        .expect("inspect fresh Git sink");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "authority denial must precede the first output object"
    );
}

#[test]
fn public_export_rejects_retained_original_before_emitting_any_git_object() {
    let f = fixture();
    let plan = prepare(&f);
    let denied = plan.historical_originals()[0]
        .verify()
        .expect("valid retained original")
        .id()
        .expect("ID");
    let sink =
        GitRepository::init_bare(f._directory.path().join("denied.git")).expect("fresh sink");
    let result = export_public_git_history_with_authority(
        &f.native,
        &sink,
        &[HistoryTip {
            thread: "main",
            state: f.base,
        }],
        &["main"],
        ViewLimits::default(),
        |_, operation| {
            if operation.id()? == denied {
                return Err(GitProjectionError::Git(
                    "retained author no longer authorized".into(),
                ));
            }
            Ok(())
        },
    );
    assert!(
        result.is_err(),
        "an admitted history is still subject to current authority"
    );
    assert_empty(&sink);
}

#[test]
fn successful_earlier_export_does_not_turn_a_retained_original_into_cached_authority() {
    let f = fixture();
    let first =
        GitRepository::init_bare(f._directory.path().join("first.git")).expect("fresh sink");
    let seen = RefCell::new(BTreeSet::new());
    let mapping = export_public_git_history_with_authority(
        &f.native,
        &first,
        &[HistoryTip {
            thread: "main",
            state: f.base,
        }],
        &["main"],
        ViewLimits::default(),
        |replica, operation| {
            replica
                .verify_local_source_owner(operation)
                .map_err(|error| GitProjectionError::Git(error.to_string()))?;
            seen.borrow_mut().insert(operation.id()?);
            Ok(())
        },
    )
    .expect("authorized initial export");
    assert_eq!(mapping.get_git(&f.base), Some(f.old));
    assert_eq!(seen.borrow().len(), 2, "both old authors must be checked");
    let second = GitRepository::init_bare(f._directory.path().join("revoked.git"))
        .expect("fresh retry sink");
    let error = export_public_git_history_with_authority(
        &f.native,
        &second,
        &[HistoryTip {
            thread: "main",
            state: f.base,
        }],
        &["main"],
        ViewLimits::default(),
        |_, _| {
            Err(GitProjectionError::Git(
                "authoritative registry unavailable".into(),
            ))
        },
    )
    .expect_err("cached projection cannot conceal unavailable current authority");
    assert!(error.to_string().contains("registry unavailable"));
    assert_empty(&second);
}
