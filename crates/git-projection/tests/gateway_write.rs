// SPDX-License-Identifier: Apache-2.0
//! Local fixture acceptance is durable native history, never a writable mirror.
use heddle_git_projection::{
    gateway_view::{HistoryTip, ViewLimits, export_public_git_history},
    gateway_write::{
        LocalPush, LocalWriter, PushAuthor, PushUpdate, WriteLimits, accept_local_fixture_push,
        parse_receive_pack, prepare_git_push,
    },
};
use objects::{
    object::{Attribution, Principal, StateId},
    store::ObjectStore,
};
use repo::Repository;
use sley::{GitObjectType, ObjectId};

fn fixture() -> (
    tempfile::TempDir,
    Repository,
    StateId,
    sley::Repository,
    ObjectId,
) {
    let root = tempfile::tempdir().expect("fixture");
    let native = Repository::init_default(root.path().join("native")).expect("native repository");
    std::fs::write(native.root().join("base.txt"), b"base\n").expect("file");
    let base = native
        .snapshot_with_attribution(
            Some("base".into()),
            None,
            Attribution::human(Principal::new("Fixture", "fixture@example.invalid")),
        )
        .expect("base")
        .state_id;
    let git = sley::Repository::init_bare(root.path().join("quarantine.git")).expect("quarantine");
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
    .expect("public base");
    let old = mapping.get_git(&base).expect("old Git tip");
    (root, native, base, git, old)
}
fn commit(git: &sley::Repository, parent: ObjectId, message: &str) -> ObjectId {
    let blob = git
        .write_raw_object(GitObjectType::Blob, format!("{message}\n").into_bytes())
        .expect("blob");
    let mut tree = b"100644 source.txt\0".to_vec();
    tree.extend(blob.as_bytes());
    let tree = git
        .write_raw_object(GitObjectType::Tree, tree)
        .expect("tree");
    let commit = format!(
        "tree {tree}\nparent {parent}\nauthor Untrusted Git Author <git@example.invalid> 1700000000 +0000\ncommitter Git Committer <committer@example.invalid> 1700000001 +0000\n\n{message}\n"
    );
    git.write_raw_object(GitObjectType::Commit, commit.into_bytes())
        .expect("commit")
}
#[test]
fn multi_commit_push_has_signed_native_history_and_survives_restart_without_quarantine() {
    let (root, native, base, git, old) = fixture();
    let one = commit(&git, old, "first Git commit");
    let two = commit(&git, one, "second Git commit");
    let update = PushUpdate {
        thread: "main".into(),
        old,
        new: two,
    };
    let replica = native.native_thread("main").expect("Thread");
    let signer = native
        .native_thread_signer(&replica)
        .expect("existing local signer");
    let request = || LocalPush {
        update: &update,
        expected_native: base,
        policy_generation: "synthetic-policy-1",
    };
    let writer = || LocalWriter {
        actor: "synthetic-writer",
        signer: &signer,
    };
    let accepted = accept_local_fixture_push(
        &native,
        &git,
        request(),
        writer(),
        WriteLimits::default(),
        || Ok(()),
    )
    .expect("native acceptance");
    assert!(!accepted.replayed);
    assert_eq!(accepted.receipt.operations.len(), 2);
    assert_eq!(
        replica.projection().expect("view").source_heads,
        [accepted.receipt.native_state]
    );
    let state = native
        .store()
        .get_state(&accepted.receipt.native_state)
        .expect("state")
        .expect("native state");
    assert!(state.raw_message.is_some());
    assert!(!state.git_lossy);
    for id in &accepted.receipt.operations {
        let (signed, admission) = replica
            .operation(id)
            .expect("read operation")
            .expect("durable signed original");
        assert_eq!(
            admission,
            objects::object::thread_replication::Admission::Accepted
        );
        assert!(
            signed
                .verify()
                .expect("real signature")
                .source_state()
                .expect("capture")
                .is_some()
        );
    }
    let generation = replica.generation().expect("generation");
    let replay = accept_local_fixture_push(
        &native,
        &git,
        request(),
        writer(),
        WriteLimits::default(),
        || Ok(()),
    )
    .expect("lost ACK replay");
    assert!(replay.replayed);
    assert_eq!(replay.receipt, accepted.receipt);
    assert_eq!(replica.generation().expect("stable generation"), generation);
    drop(git);
    drop(native);
    std::fs::remove_dir_all(root.path().join("quarantine.git")).expect("discard Git input");
    let native = Repository::open(root.path().join("native")).expect("reopened native source");
    let fresh = sley::Repository::init_bare(root.path().join("fresh.git")).expect("empty sink");
    let mapping = export_public_git_history(
        &native,
        &fresh,
        &[HistoryTip {
            thread: "main",
            state: accepted.receipt.native_state,
        }],
        &["main"],
        ViewLimits::default(),
    )
    .expect("fresh native-derived Git history");
    assert_eq!(mapping.get_git(&accepted.receipt.native_state), Some(two));
    assert!(fresh.read_commit(&one).is_ok());
    assert!(fresh.read_commit(&old).is_ok());
}
#[test]
fn final_authorization_failure_rolls_back_native_heads_and_receipt() {
    let (_root, native, base, git, old) = fixture();
    let new = commit(&git, old, "must be denied");
    let update = PushUpdate {
        thread: "main".into(),
        old,
        new,
    };
    let replica = native.native_thread("main").expect("Thread");
    let signer = native.native_thread_signer(&replica).expect("signer");
    let before = replica.generation().expect("generation");
    let checks = std::cell::Cell::new(0);
    let result = accept_local_fixture_push(
        &native,
        &git,
        LocalPush {
            update: &update,
            expected_native: base,
            policy_generation: "1",
        },
        LocalWriter {
            actor: "writer",
            signer: &signer,
        },
        WriteLimits::default(),
        || {
            checks.set(checks.get() + 1);
            if checks.get() > 1 {
                Err(heddle_git_projection::GitProjectionError::Git(
                    "writer revoked".into(),
                ))
            } else {
                Ok(())
            }
        },
    );
    assert!(
        result
            .err()
            .expect("denied")
            .to_string()
            .contains("writer revoked")
    );
    assert_eq!(
        replica.projection().expect("unchanged head").source_heads,
        [base]
    );
    assert_eq!(replica.generation().expect("unchanged generation"), before);
    let accepted = accept_local_fixture_push(
        &native,
        &git,
        LocalPush {
            update: &update,
            expected_native: base,
            policy_generation: "1",
        },
        LocalWriter {
            actor: "writer",
            signer: &signer,
        },
        WriteLimits::default(),
        || Ok(()),
    )
    .expect("fresh attempt");
    assert!(!accepted.replayed);
}
fn packet(command: &str) -> Vec<u8> {
    let mut body = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
    body.extend_from_slice(b"PACK\0\0\0\x02\0\0\0\0");
    body.extend_from_slice(&[0; 20]);
    body
}
#[test]
fn wire_accepts_one_bounded_existing_branch_and_rejects_unsupported_extensions() {
    let old = "1".repeat(40);
    let new = "2".repeat(40);
    let valid = format!(
        "{old} {new} refs/heads/main\0report-status ofs-delta object-format=sha1 agent=git/2.39.5"
    );
    assert_eq!(
        parse_receive_pack(&packet(&valid), WriteLimits::default())
            .expect("command")
            .update
            .thread,
        "main"
    );
    // Actual Git 2.47 send-pack vector observed by the smart-HTTP integration.
    let git_247 = format!("{old} {new} refs/heads/main\0 report-status object-format=sha1");
    let body = packet(&git_247);
    let parsed = parse_receive_pack(&body, WriteLimits::default()).expect("Git 2.47 command");
    assert_eq!(parsed.update.old.to_string(), old);
    assert_eq!(parsed.update.new.to_string(), new);
    assert_eq!(parsed.update.thread, "main");
    for caps in [
        "  report-status",
        "\treport-status",
        " report-status ",
        " report-status  object-format=sha1",
        "report-status push-options",
        "report-status atomic",
        "report-status side-band-64k",
        "report-status-v2",
        "report-status object-format=sha256",
        "report-status report-status",
    ] {
        let command = format!("{old} {new} refs/heads/main\0{caps}");
        assert!(
            parse_receive_pack(&packet(&command), WriteLimits::default()).is_err(),
            "{caps}"
        );
    }
    for branch in [
        "HEAD",
        "refs/tags/v1",
        "refs/notes/heddle",
        "refs/replace/main",
        "refs/heads/../main",
    ] {
        assert!(
            parse_receive_pack(
                &packet(&format!("{old} {new} {branch}\0report-status")),
                WriteLimits::default()
            )
            .is_err(),
            "{branch}"
        );
    }
    for (old, new) in [
        ("0".repeat(40), new.clone()),
        (old.clone(), "0".repeat(40)),
        (old.clone(), old),
    ] {
        assert!(
            parse_receive_pack(
                &packet(&format!("{old} {new} refs/heads/main\0report-status")),
                WriteLimits::default()
            )
            .is_err()
        );
    }
}

#[test]
fn unsigned_preparation_and_external_signing_preserve_native_acceptance_boundary() {
    use crypto::{Signer, thread_operation::SignedOperation};
    use objects::object::thread_replication::SourceAuthor;

    let (_root, native, base, git, old) = fixture();
    let one = commit(&git, old, "externally signed first");
    let two = commit(&git, one, "externally signed second");
    let update = PushUpdate {
        thread: "main".into(),
        old,
        new: two,
    };
    let replica = native.native_thread("main").expect("Thread");
    let signer = native
        .native_thread_signer(&replica)
        .expect("explicit fixture signer");
    let publisher = signer.public_key().try_into().expect("key");
    let generation = replica.generation().expect("generation");
    let request = || LocalPush {
        update: &update,
        expected_native: base,
        policy_generation: "fixture-policy-generation-1",
    };
    let authorize = |scope: &heddle_git_projection::gateway_write::PushScope| {
        assert_eq!(scope.actor, "explicit-fixture-actor");
        assert_eq!(scope.publisher, publisher);
        assert_eq!(scope.source_author, SourceAuthor::LocalKey);
        assert_eq!(scope.expected_native, base);
        assert_eq!(scope.old_git, old.to_string());
        assert_eq!(scope.new_git, two.to_string());
        assert_eq!(scope.expected_generation, generation);
        assert_eq!(scope.policy_generation, "fixture-policy-generation-1");
        Ok(())
    };
    let prepared = prepare_git_push(
        &native,
        &git,
        request(),
        PushAuthor {
            actor: "explicit-fixture-actor",
            publisher,
            source_author: &SourceAuthor::LocalKey,
        },
        WriteLimits::default(),
        authorize,
    )
    .expect("unsigned plan");
    assert_eq!(prepared.operations().len(), 2);
    assert_eq!(prepared.git_oid(&base), Some(old));
    assert_eq!(
        prepared.git_oid(&prepared.receipt().native_state),
        Some(two)
    );
    assert_eq!(prepared.states().len(), 4); // canonical seed, old base, two new commits
    let historical_count = prepared.historical_originals().len();
    assert!(historical_count > 0);
    let expected_checked: std::collections::BTreeSet<_> = prepared
        .historical_originals()
        .iter()
        .map(|original| original.canonical.clone())
        .chain(
            prepared
                .operations()
                .iter()
                .map(|operation| operation.encode().expect("canonical operation")),
        )
        .collect();
    let receipt = prepared.receipt().clone();
    for operation in &receipt.operations {
        assert!(
            replica
                .operation(operation)
                .expect("operation lookup")
                .is_none()
        );
    }
    assert_eq!(replica.generation().expect("no admission"), generation);
    assert_eq!(replica.projection().expect("head").source_heads, [base]);

    // The caller owns signing. No signer or credential was passed to preparation.
    let originals = prepared
        .operations()
        .iter()
        .map(|operation| SignedOperation::sign(operation, &signer).expect("external signer"))
        .collect();
    let checks = std::cell::Cell::new(0);
    let checked = std::cell::RefCell::new(std::collections::BTreeSet::new());
    let signed = prepared
        .bind_signed(&native, originals, authorize, |replica, operation| {
            checks.set(checks.get() + 1);
            checked
                .borrow_mut()
                .insert(operation.encode().expect("verified canonical operation"));
            replica
                .verify_local_source_owner(operation)
                .map_err(|e| heddle_git_projection::GitProjectionError::Git(e.to_string()))
        })
        .expect("fresh sender binding");
    // Current authority covers every retained original as well as both new
    // captures. Checking only the two incoming operations misses old revocation.
    assert_eq!(checks.get(), expected_checked.len());
    assert_eq!(checked.into_inner(), expected_checked);
    assert_eq!(signed.receipt(), &receipt);
    assert_eq!(signed.originals().len(), 2);
    assert_eq!(signed.source_originals().len(), historical_count + 2);
    assert_eq!(signed.git_oid(&receipt.native_state), Some(two));
    assert!(signed.require_hosted_git_acceptance().is_err());
    assert_eq!(
        replica.generation().expect("binding is not admission"),
        generation
    );
    for operation in &receipt.operations {
        assert!(
            replica
                .operation(operation)
                .expect("operation lookup")
                .is_none()
        );
    }
    let unpublished =
        sley::Repository::init_bare(native.root().join("unpublished.git")).expect("sink");
    assert!(
        export_public_git_history(
            &native,
            &unpublished,
            &[HistoryTip {
                thread: "main",
                state: receipt.native_state
            }],
            &["main"],
            ViewLimits::default()
        )
        .is_err()
    );

    // The existing durable local adapter still accepts the same exact command,
    // proving extraction preserved original identities and lost-ACK semantics.
    let accepted = accept_local_fixture_push(
        &native,
        &git,
        request(),
        LocalWriter {
            actor: "explicit-fixture-actor",
            signer: &signer,
        },
        WriteLimits::default(),
        || Ok(()),
    )
    .expect("real local native CAS");
    assert_eq!(accepted.receipt, receipt);
    assert!(!accepted.replayed);
    let replay = accept_local_fixture_push(
        &native,
        &git,
        request(),
        LocalWriter {
            actor: "explicit-fixture-actor",
            signer: &signer,
        },
        WriteLimits::default(),
        || Ok(()),
    )
    .expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.receipt, receipt);
}
